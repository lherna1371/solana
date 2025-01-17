//! Defines a transaction which supports multiple versions of messages.

#![cfg(feature = "full")]

use {
    crate::{
        hash::Hash,
        message::VersionedMessage,
        sanitize::SanitizeError,
        short_vec,
        signature::Signature,
        signer::SignerError,
        signers::Signers,
        transaction::{Result, Transaction, TransactionError},
    },
    serde::Serialize,
    std::cmp::Ordering,
};

mod sanitized;

pub use sanitized::*;

/// Type that serializes to the string "legacy"
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Legacy {
    Legacy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", untagged)]
pub enum TransactionVersion {
    Legacy(Legacy),
    Number(u8),
}

impl TransactionVersion {
    pub const LEGACY: Self = Self::Legacy(Legacy::Legacy);
}

// NOTE: Serialization-related changes must be paired with the direct read at sigverify.
/// An atomic transaction
#[derive(Debug, PartialEq, Default, Eq, Clone, Serialize, Deserialize, AbiExample)]
pub struct VersionedTransaction {
    /// List of signatures
    #[serde(with = "short_vec")]
    pub signatures: Vec<Signature>,
    /// Message to sign.
    pub message: VersionedMessage,
}

impl From<Transaction> for VersionedTransaction {
    fn from(transaction: Transaction) -> Self {
        Self {
            signatures: transaction.signatures,
            message: VersionedMessage::Legacy(transaction.message),
        }
    }
}

impl VersionedTransaction {
    /// Signs a versioned message and if successful, returns a signed
    /// transaction.
    pub fn try_new<T: Signers>(
        message: VersionedMessage,
        keypairs: &T,
    ) -> std::result::Result<Self, SignerError> {
        let static_account_keys = message.static_account_keys();
        if static_account_keys.len() < message.header().num_required_signatures as usize {
            return Err(SignerError::InvalidInput("invalid message".to_string()));
        }

        let signer_keys = keypairs.try_pubkeys()?;
        let expected_signer_keys =
            &static_account_keys[0..message.header().num_required_signatures as usize];

        match signer_keys.len().cmp(&expected_signer_keys.len()) {
            Ordering::Greater => Err(SignerError::TooManySigners),
            Ordering::Less => Err(SignerError::NotEnoughSigners),
            Ordering::Equal => Ok(()),
        }?;

        let message_data = message.serialize();
        let signature_indexes: Vec<usize> = expected_signer_keys
            .iter()
            .map(|signer_key| {
                signer_keys
                    .iter()
                    .position(|key| key == signer_key)
                    .ok_or(SignerError::KeypairPubkeyMismatch)
            })
            .collect::<std::result::Result<_, SignerError>>()?;

        let unordered_signatures = keypairs.try_sign_message(&message_data)?;
        let signatures: Vec<Signature> = signature_indexes
            .into_iter()
            .map(|index| {
                unordered_signatures
                    .get(index)
                    .copied()
                    .ok_or_else(|| SignerError::InvalidInput("invalid keypairs".to_string()))
            })
            .collect::<std::result::Result<_, SignerError>>()?;

        Ok(Self {
            signatures,
            message,
        })
    }

    pub fn sanitize(
        &self,
        require_static_program_ids: bool,
    ) -> std::result::Result<(), SanitizeError> {
        self.message.sanitize(require_static_program_ids)?;
        self.sanitize_signatures()?;
        Ok(())
    }

    pub(crate) fn sanitize_signatures(&self) -> std::result::Result<(), SanitizeError> {
        let num_required_signatures = usize::from(self.message.header().num_required_signatures);
        match num_required_signatures.cmp(&self.signatures.len()) {
            Ordering::Greater => Err(SanitizeError::IndexOutOfBounds),
            Ordering::Less => Err(SanitizeError::InvalidValue),
            Ordering::Equal => Ok(()),
        }?;

        // Signatures are verified before message keys are loaded so all signers
        // must correspond to static account keys.
        if self.signatures.len() > self.message.static_account_keys().len() {
            return Err(SanitizeError::IndexOutOfBounds);
        }

        Ok(())
    }

    /// Returns the version of the transaction
    pub fn version(&self) -> TransactionVersion {
        match self.message {
            VersionedMessage::Legacy(_) => TransactionVersion::LEGACY,
            VersionedMessage::V0(_) => TransactionVersion::Number(0),
        }
    }

    /// Returns a legacy transaction if the transaction message is legacy.
    pub fn into_legacy_transaction(self) -> Option<Transaction> {
        match self.message {
            VersionedMessage::Legacy(message) => Some(Transaction {
                signatures: self.signatures,
                message,
            }),
            _ => None,
        }
    }

    /// Verify the transaction and hash its message
    pub fn verify_and_hash_message(&self) -> Result<Hash> {
        let message_bytes = self.message.serialize();
        if !self
            ._verify_with_results(&message_bytes)
            .iter()
            .all(|verify_result| *verify_result)
        {
            Err(TransactionError::SignatureFailure)
        } else {
            Ok(VersionedMessage::hash_raw_message(&message_bytes))
        }
    }

    /// Verify the transaction and return a list of verification results
    pub fn verify_with_results(&self) -> Vec<bool> {
        let message_bytes = self.message.serialize();
        self._verify_with_results(&message_bytes)
    }

    fn _verify_with_results(&self, message_bytes: &[u8]) -> Vec<bool> {
        self.signatures
            .iter()
            .zip(self.message.static_account_keys().iter())
            .map(|(signature, pubkey)| signature.verify(pubkey.as_ref(), message_bytes))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            message::Message as LegacyMessage,
            signer::{keypair::Keypair, Signer},
        },
        solana_program::{
            instruction::{AccountMeta, Instruction},
            pubkey::Pubkey,
        },
    };

    #[test]
    fn test_try_new() {
        let keypair0 = Keypair::new();
        let keypair1 = Keypair::new();
        let keypair2 = Keypair::new();

        let message = VersionedMessage::Legacy(LegacyMessage::new(
            &[Instruction::new_with_bytes(
                Pubkey::new_unique(),
                &[],
                vec![
                    AccountMeta::new_readonly(keypair1.pubkey(), true),
                    AccountMeta::new_readonly(keypair2.pubkey(), false),
                ],
            )],
            Some(&keypair0.pubkey()),
        ));

        assert_eq!(
            VersionedTransaction::try_new(message.clone(), &[&keypair0]),
            Err(SignerError::NotEnoughSigners)
        );

        assert_eq!(
            VersionedTransaction::try_new(message.clone(), &[&keypair0, &keypair0]),
            Err(SignerError::KeypairPubkeyMismatch)
        );

        assert_eq!(
            VersionedTransaction::try_new(message.clone(), &[&keypair1, &keypair2]),
            Err(SignerError::KeypairPubkeyMismatch)
        );

        match VersionedTransaction::try_new(message.clone(), &[&keypair0, &keypair1]) {
            Ok(tx) => assert_eq!(tx.verify_with_results(), vec![true; 2]),
            Err(err) => assert_eq!(Some(err), None),
        }

        match VersionedTransaction::try_new(message, &[&keypair1, &keypair0]) {
            Ok(tx) => assert_eq!(tx.verify_with_results(), vec![true; 2]),
            Err(err) => assert_eq!(Some(err), None),
        }
    }
}
