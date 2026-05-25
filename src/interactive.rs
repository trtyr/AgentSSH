use crate::cli::ExpectRespondPair;
use anyhow::{Result, bail};

pub fn expect_pairs(
    pairs: [(Option<String>, Option<String>); 3],
) -> Result<Vec<ExpectRespondPair>> {
    let mut extracted = Vec::new();
    for (expect, respond) in pairs {
        match (expect, respond) {
            (Some(expect), Some(respond)) => extracted.push(ExpectRespondPair { expect, respond }),
            (None, None) => {}
            (Some(_), None) => bail!("--expect requires matching --respond"),
            (None, Some(_)) => bail!("--respond requires matching --expect"),
        }
    }
    Ok(extracted)
}
