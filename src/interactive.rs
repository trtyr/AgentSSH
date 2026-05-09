use crate::cli::ExpectRespondPair;
use crate::session::RemoteSession;
use anyhow::{Result, bail};
use regex::RegexBuilder;

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

pub fn output_matches(session: &RemoteSession, pattern: &str) -> Result<bool> {
    let haystack = String::from_utf8_lossy(&session.output).into_owned();
    let haystack_lower = haystack.to_lowercase();
    let pattern_lower = pattern.to_lowercase();
    let regex = RegexBuilder::new(pattern).case_insensitive(true).build();
    match regex {
        Ok(compiled) => Ok(compiled.is_match(&haystack) || haystack_lower.contains(&pattern_lower)),
        Err(_) => Ok(haystack_lower.contains(&pattern_lower)),
    }
}
