//! Format negotiation (DESIGN §4). Negotiation lives in capmeshd (the daemon only confirms
//! it can honour the chosen format), so the algorithm is here, transport-agnostic.
//!
//! §4.1: from the consuming side's preference-ordered formats and the producing side's
//! formats, pick the top-ranked format compatible with both. M0 does **direct** matching
//! only — equal `codec` and equal params (§4.2 exact-match). Convertible params
//! (`midi1`↔`ump`, audio rate/channels) need a converter capability and are a later
//! milestone (§4.3); until then differing formats are incompatible. An empty compatible set
//! is refused with [`NoCommonFormat`] (§4.1 step 4), carrying both sides for diagnosis.

use capmesh_ctl::Format;

/// No format is compatible with both sides (§4.1 step 4 → the `no-common-format` error).
#[derive(Debug, Clone, PartialEq)]
pub struct NoCommonFormat {
    /// The consuming side's formats (preference-ordered).
    pub consumer: Vec<Format>,
    /// The producing side's formats.
    pub producer: Vec<Format>,
}

/// Pick the top-ranked format compatible with both sides, ranked by the **consumer's**
/// preference order (§4.1). Direct match only in M0 (equal codec + params).
pub fn negotiate(consumer: &[Format], producer: &[Format]) -> Result<Format, NoCommonFormat> {
    for want in consumer {
        if producer.contains(want) {
            return Ok(want.clone());
        }
    }
    Err(NoCommonFormat {
        consumer: consumer.to_vec(),
        producer: producer.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(codec: &str) -> Format {
        Format {
            codec: codec.to_string(),
            params: Default::default(),
        }
    }

    #[test]
    fn trivial_midi_case_picks_midi1() {
        assert_eq!(negotiate(&[f("midi1")], &[f("midi1")]).unwrap(), f("midi1"));
    }

    #[test]
    fn skips_a_consumer_preference_the_producer_lacks() {
        // Consumer prefers ump but the producer only does midi1 → falls to midi1.
        let chosen = negotiate(&[f("ump"), f("midi1")], &[f("midi1")]).unwrap();
        assert_eq!(chosen, f("midi1"));
    }

    #[test]
    fn ranks_by_consumer_preference_not_producer() {
        // Both support both; the consumer's order (midi1 first) wins.
        let chosen = negotiate(&[f("midi1"), f("ump")], &[f("ump"), f("midi1")]).unwrap();
        assert_eq!(chosen, f("midi1"));
    }

    #[test]
    fn disjoint_codecs_have_no_common_format() {
        let err = negotiate(&[f("ump")], &[f("midi1")]).unwrap_err();
        assert_eq!(err.consumer, vec![f("ump")]);
        assert_eq!(err.producer, vec![f("midi1")]);
    }

    #[test]
    fn same_codec_differing_params_is_incompatible_in_m0() {
        // ump group 0 vs group 1 — no converter in M0, so not directly compatible.
        let g0 = Format {
            codec: "ump".into(),
            params: serde_json::Map::from_iter([("group".to_string(), serde_json::json!(0))]),
        };
        let g1 = Format {
            codec: "ump".into(),
            params: serde_json::Map::from_iter([("group".to_string(), serde_json::json!(1))]),
        };
        use std::slice::from_ref;
        assert!(negotiate(from_ref(&g0), from_ref(&g1)).is_err());
        // Identical params negotiate fine.
        assert_eq!(negotiate(from_ref(&g0), from_ref(&g0)).unwrap(), g0);
    }
}
