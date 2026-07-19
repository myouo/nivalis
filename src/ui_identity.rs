use slint::SharedString;
use std::num::NonZeroI64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EntityKey(NonZeroI64);

impl EntityKey {
    pub(crate) fn new(value: i64) -> Option<Self> {
        NonZeroI64::new(value)
            .filter(|value| value.get() > 0)
            .map(Self)
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        if value.is_empty()
            || value.starts_with('0')
            || !value.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        Self::new(value.parse().ok()?)
    }

    pub(crate) fn get(self) -> i64 {
        self.0.get()
    }

    pub(crate) fn encode(self) -> SharedString {
        self.get().to_string().into()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountKey {
    All,
    Account(EntityKey),
}

impl AccountKey {
    pub(crate) fn from_scope_id(value: i64) -> Option<Self> {
        if value == 0 {
            Some(Self::All)
        } else {
            EntityKey::new(value).map(Self::Account)
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        if value.is_empty() {
            Some(Self::All)
        } else {
            EntityKey::parse(value).map(Self::Account)
        }
    }

    pub(crate) fn encode(self) -> SharedString {
        match self {
            Self::All => SharedString::default(),
            Self::Account(id) => id.encode(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maximum_database_identity_round_trips_without_truncation() {
        let key = EntityKey::new(i64::MAX).unwrap();
        let encoded = key.encode();

        assert_eq!(encoded.as_str(), "9223372036854775807");
        assert_eq!(EntityKey::parse(encoded.as_str()), Some(key));
    }

    #[test]
    fn entity_identity_rejects_noncanonical_or_invalid_values() {
        for invalid in [
            "",
            "0",
            "00",
            "01",
            "-1",
            "+1",
            " 1",
            "1 ",
            "1.0",
            "mail-1",
            "9223372036854775808",
        ] {
            assert_eq!(EntityKey::parse(invalid), None, "accepted {invalid:?}");
        }
    }

    #[test]
    fn empty_account_key_means_all_accounts_only() {
        assert_eq!(AccountKey::parse(""), Some(AccountKey::All));
        assert_eq!(AccountKey::from_scope_id(0), Some(AccountKey::All));
        assert_eq!(AccountKey::All.encode().as_str(), "");

        let account = EntityKey::new(i64::MAX).unwrap();
        assert_eq!(
            AccountKey::parse(AccountKey::Account(account).encode().as_str()),
            Some(AccountKey::Account(account))
        );
        assert_eq!(AccountKey::from_scope_id(-1), None);
    }
}
