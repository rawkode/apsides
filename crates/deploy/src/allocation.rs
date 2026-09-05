use crate::{Error, GenerationAllocation, Result, valid_digest};

/// Injective Kubernetes identities derived from one registry incarnation and reservation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllocationNames {
    pub generation_id: String,
    pub transition_id: String,
    pub service_account_name: String,
}

/// Derive bounded identities using all 128 UUID bits and a canonical base36 sequence.
/// # Errors
/// Rejects noncanonical Kubernetes UUIDs, zero sequences and exhausted counters.
pub fn allocation_names(registry_uid: &str, sequence: u64) -> Result<AllocationNames> {
    let compact: String = registry_uid.chars().filter(|c| *c != '-').collect();
    if registry_uid.len() != 36
        || compact.len() != 32
        || registry_uid.bytes().enumerate().any(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b != b'-'
            } else {
                !b.is_ascii_digit() && !(b'a'..=b'f').contains(&b)
            }
        })
        || sequence == 0
        || sequence == u64::MAX
    {
        return Err(Error::Plan(
            "invalid registry UUID or exhausted allocation sequence".into(),
        ));
    }
    let uuid = u128::from_str_radix(&compact, 16)
        .map_err(|_| Error::Plan("invalid registry UUID".into()))?;
    let mut encoded = String::with_capacity(26);
    let mut buffer = 0_u16;
    let mut bits = 0_u8;
    for byte in uuid.to_be_bytes() {
        buffer = (buffer << 8) | u16::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            encoded.push(base32_digit(((buffer >> bits) & 31) as u8));
        }
        buffer &= (1 << bits) - 1;
    }
    if bits > 0 {
        encoded.push(base32_digit(((buffer << (5 - bits)) & 31) as u8));
    }
    let token = format!("{encoded}-{}", base36(sequence));
    Ok(AllocationNames {
        generation_id: format!("g{token}"),
        transition_id: format!("t{token}"),
        service_account_name: format!("r{token}"),
    })
}

fn base32_digit(digit: u8) -> char {
    char::from(if digit < 26 {
        b'a' + digit
    } else {
        b'2' + digit - 26
    })
}
fn base36(mut value: u64) -> String {
    let mut digits = Vec::new();
    while value > 0 {
        let digit = (value % 36) as u8;
        digits.push(char::from(if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        }));
        value /= 36;
    }
    digits.into_iter().rev().collect()
}

/// Check the complete allocation identity and its metadata commitments.
/// # Errors
/// Rejects noncanonical names or invalid digest metadata.
pub fn validate_allocation(allocation: &GenerationAllocation) -> Result<()> {
    if allocation.inventory_metadata_digest.is_none() {
        return Err(Error::Plan(
            "protected inventory checkpoint is missing; explicit recovery is required".into(),
        ));
    }
    let names = allocation_names(&allocation.registry_uid, allocation.sequence)?;
    if allocation.generation_id != names.generation_id
        || allocation.transition_id != names.transition_id
        || allocation.service_account_name != names.service_account_name
        || !valid_digest(&allocation.artifact_digest)
        || !valid_digest(&allocation.ir_digest)
        || allocation
            .inventory_metadata_digest
            .as_ref()
            .is_none_or(|value| !valid_digest(value))
    {
        return Err(Error::Plan(
            "allocation identity or metadata commitment is invalid".into(),
        ));
    }
    Ok(())
}

pub(crate) fn service_account_for_generation(generation: &str) -> Result<String> {
    let token = generation
        .strip_prefix('g')
        .ok_or_else(|| Error::Plan("noncanonical generation identity".into()))?;
    let (encoded, sequence) = token
        .split_once('-')
        .ok_or_else(|| Error::Plan("noncanonical generation identity".into()))?;
    let sequence_value = u64::from_str_radix(sequence, 36)
        .map_err(|_| Error::Plan("invalid allocation sequence".into()))?;
    if encoded.len() != 26
        || sequence_value == 0
        || sequence_value == u64::MAX
        || base36(sequence_value) != sequence
        || encoded
            .bytes()
            .any(|b| !b.is_ascii_lowercase() && !(b'2'..=b'7').contains(&b))
        || encoded
            .bytes()
            .last()
            .is_none_or(|b| !matches!(b, b'a' | b'e' | b'i' | b'm' | b'q' | b'u' | b'y' | b'4'))
    {
        return Err(Error::Plan("noncanonical generation identity".into()));
    }
    Ok(format!("r{token}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_preserve_incarnation_and_sequence_without_suffix_overflow() -> Result<()> {
        let uid = "11111111-1111-4111-8111-111111111111";
        let names = allocation_names(uid, 1)?;
        assert_eq!(names.generation_id, "gceirceircfardairceirceirce-1");
        assert_eq!(names.transition_id, "tceirceircfardairceirceirce-1");
        assert_eq!(names.service_account_name, "rceirceircfardairceirceirce-1");
        for sequence in [1, 35, 36, u64::MAX - 1] {
            let names = allocation_names(uid, sequence)?;
            assert_eq!(
                service_account_for_generation(&names.generation_id)?,
                names.service_account_name
            );
            assert!(format!("reakt-proxy-{}-config", names.generation_id).len() <= 63);
            assert_ne!(
                names,
                allocation_names("11111111-1111-4111-8111-111111111112", sequence)?
            );
        }
        assert_ne!(allocation_names(uid, 35)?, allocation_names(uid, 36)?);
        Ok(())
    }

    #[test]
    fn noncanonical_and_exhausted_allocations_reject() {
        let uid = "11111111-1111-4111-8111-111111111111";
        assert!(allocation_names(uid, 0).is_err());
        assert!(allocation_names(uid, u64::MAX).is_err());
        assert!(allocation_names("11111111-1111-4111-8111-11111111111A", 1).is_err());
        assert!(allocation_names("11111111111141118111111111111111", 1).is_err());
        for generation in [
            "generation-a",
            "gceirceircfardairceirceirce-01",
            "gceirceircfardairceirceircf-1",
        ] {
            assert!(service_account_for_generation(generation).is_err());
        }
    }
}
