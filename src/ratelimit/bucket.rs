//! Matemática do token bucket.
//!
//! Isolada do armazenamento de propósito: é a mesma conta na implementação
//! in-memory e no script Lua do Redis, e é o que a conformance suite compara.

/// Estado persistido de um bucket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BucketState {
    pub tokens: f64,
    pub last_refill_ms: u64,
}

impl BucketState {
    pub fn full(capacity: u32, now_ms: u64) -> Self {
        Self {
            tokens: capacity as f64,
            last_refill_ms: now_ms,
        }
    }
}

/// Recalcula o refill preguiçosamente pelo tempo decorrido.
///
/// Preguiçoso e não por timer: um bucket só precisa estar correto no instante em
/// que alguém pergunta, e manter um timer por chave custaria um scheduler inteiro
/// para produzir a mesma resposta.
pub fn refill(state: BucketState, capacity: u32, refill_per_sec: f64, now_ms: u64) -> BucketState {
    // Relógio para trás (réplica ressincronizando, ou o `TIME` do Redis recuando)
    // não pode virar crédito nem débito: o bucket apenas não envelhece.
    let elapsed_ms = now_ms.saturating_sub(state.last_refill_ms);
    let gained = (elapsed_ms as f64 / 1000.0) * refill_per_sec;

    BucketState {
        tokens: (state.tokens + gained).min(capacity as f64),
        last_refill_ms: now_ms.max(state.last_refill_ms),
    }
}

/// Tempo até haver `needed` tokens, em milissegundos. Base do `Retry-After`.
pub fn time_to_tokens_ms(tokens: f64, needed: f64, refill_per_sec: f64) -> u64 {
    if tokens >= needed {
        return 0;
    }
    if refill_per_sec <= 0.0 {
        return u64::MAX;
    }
    (((needed - tokens) / refill_per_sec) * 1000.0).ceil() as u64
}

/// TTL do bucket: o tempo necessário para enchê-lo do zero.
///
/// Sem TTL, todo IP que já passou pelo gateway fica residente para sempre. Com
/// ele, um bucket cheio é indistinguível de um bucket que nunca existiu, e some.
pub fn ttl_ms(capacity: u32, refill_per_sec: f64) -> u64 {
    if refill_per_sec <= 0.0 {
        return u64::MAX;
    }
    ((capacity as f64 / refill_per_sec) * 1000.0).ceil() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPACITY: u32 = 10;
    const RATE: f64 = 1.0;

    #[test]
    fn o_refill_e_proporcional_ao_tempo_decorrido() {
        let state = BucketState {
            tokens: 0.0,
            last_refill_ms: 0,
        };
        let refilled = refill(state, CAPACITY, RATE, 3_000);

        assert_eq!(refilled.tokens, 3.0);
        assert_eq!(refilled.last_refill_ms, 3_000);
    }

    #[test]
    fn o_refill_nunca_ultrapassa_a_capacidade() {
        let state = BucketState {
            tokens: 8.0,
            last_refill_ms: 0,
        };
        let refilled = refill(state, CAPACITY, RATE, 60_000);

        assert_eq!(
            refilled.tokens, CAPACITY as f64,
            "burst limitado pela capacidade"
        );
    }

    #[test]
    fn relogio_para_tras_nao_credita_nem_debita() {
        let state = BucketState {
            tokens: 5.0,
            last_refill_ms: 10_000,
        };
        let refilled = refill(state, CAPACITY, RATE, 9_000);

        assert_eq!(refilled.tokens, 5.0);
        assert_eq!(refilled.last_refill_ms, 10_000, "o timestamp não regride");
    }

    #[test]
    fn taxa_fracionaria_acumula_ao_longo_do_tempo() {
        // 0.1/s: um token a cada 10 segundos.
        let state = BucketState {
            tokens: 0.0,
            last_refill_ms: 0,
        };
        assert_eq!(refill(state, 5, 0.1, 5_000).tokens, 0.5);
        assert_eq!(refill(state, 5, 0.1, 10_000).tokens, 1.0);
    }

    #[test]
    fn tempo_ate_o_proximo_token_alimenta_o_retry_after() {
        assert_eq!(time_to_tokens_ms(3.0, 1.0, RATE), 0, "já há tokens");
        assert_eq!(time_to_tokens_ms(0.0, 1.0, RATE), 1_000);
        assert_eq!(time_to_tokens_ms(0.0, 1.0, 0.1), 10_000);
        assert_eq!(time_to_tokens_ms(0.5, 1.0, 1.0), 500);
    }

    #[test]
    fn o_ttl_e_o_tempo_de_encher_o_bucket_do_zero() {
        assert_eq!(ttl_ms(10, 1.0), 10_000);
        assert_eq!(ttl_ms(5, 0.1), 50_000);
    }
}
