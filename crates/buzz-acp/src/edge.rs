//! Optional loopback route for persistent kind-9 channel traffic.

use nostr::{Alphabet, Filter, Kind, SingleLetterTag};
use uuid::Uuid;

const MESSAGE_KIND: u16 = 9;

/// Community binding declared to the single-community edge sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EdgeBinding {
    websocket_url: String,
    http_url: String,
    canonical_origin: String,
    community_id: Uuid,
}

impl EdgeBinding {
    /// Read the optional edge route. Incomplete or invalid configuration is
    /// canonical-only, preserving the environment-variable rollback.
    pub(crate) fn from_env(canonical_relay_url: &str) -> Option<Self> {
        let edge_url = std::env::var("BUZZ_EDGE_RELAY_URL").ok()?;
        let community_id = std::env::var("BUZZ_COMMUNITY_ID")
            .ok()?
            .trim()
            .parse()
            .ok()?;
        Self::new(&edge_url, canonical_relay_url, community_id)
    }

    pub(crate) fn new(
        edge_url: &str,
        canonical_relay_url: &str,
        community_id: Uuid,
    ) -> Option<Self> {
        let mut edge = url::Url::parse(edge_url.trim()).ok()?;
        let is_loopback = match edge.host() {
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
            None => false,
        };
        if !matches!(edge.scheme(), "ws" | "http")
            || !is_loopback
            || !edge.username().is_empty()
            || edge.password().is_some()
            || (edge.path() != "" && edge.path() != "/")
            || edge.query().is_some()
            || edge.fragment().is_some()
        {
            return None;
        }
        edge.set_path("");
        let websocket_url = match edge.scheme() {
            "ws" => edge.as_str().trim_end_matches('/').to_string(),
            "http" => {
                edge.set_scheme("ws").ok()?;
                edge.as_str().trim_end_matches('/').to_string()
            }
            _ => return None,
        };
        edge.set_scheme("http").ok()?;
        let http_url = edge.as_str().trim_end_matches('/').to_string();

        let mut canonical = url::Url::parse(canonical_relay_url.trim()).ok()?;
        match canonical.scheme() {
            "ws" | "wss" => {}
            "http" => canonical.set_scheme("ws").ok()?,
            "https" => canonical.set_scheme("wss").ok()?,
            _ => return None,
        }
        if !canonical.username().is_empty()
            || canonical.password().is_some()
            || (canonical.path() != "" && canonical.path() != "/")
            || canonical.query().is_some()
            || canonical.fragment().is_some()
        {
            return None;
        }
        canonical.set_path("");

        Some(Self {
            websocket_url,
            http_url,
            canonical_origin: canonical.as_str().trim_end_matches('/').to_string(),
            community_id,
        })
    }

    pub(crate) fn websocket_url(&self) -> &str {
        &self.websocket_url
    }

    pub(crate) fn http_url(&self) -> &str {
        &self.http_url
    }

    pub(crate) fn canonical_origin(&self) -> &str {
        &self.canonical_origin
    }

    pub(crate) fn community_id(&self) -> Uuid {
        self.community_id
    }

    pub(crate) fn handshake_frame(&self) -> String {
        serde_json::json!([
            "BUZZ-EDGE",
            "BIND",
            {
                "canonical_origin": self.canonical_origin,
                "community_id": self.community_id,
            }
        ])
        .to_string()
    }
}

/// True only for the exact HTTP filter subset the phase-1 sidecar accepts.
pub(crate) fn filters_are_message_only(filters: &[Filter]) -> bool {
    let h = SingleLetterTag::lowercase(Alphabet::H);
    !filters.is_empty()
        && filters.iter().all(|filter| {
            let message_only = filter.kinds.as_ref().is_some_and(|kinds| {
                kinds.len() == 1 && kinds.contains(&Kind::Custom(MESSAGE_KIND))
            });
            let channel_scoped = filter.generic_tags.get(&h).is_some_and(|channels| {
                !channels.is_empty()
                    && channels
                        .iter()
                        .all(|channel| Uuid::parse_str(channel.as_str()).is_ok())
            });
            message_only && channel_scoped
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_requires_plain_loopback_edge_and_plain_canonical_origin() {
        let community = Uuid::new_v4();
        let binding = EdgeBinding::new("ws://127.0.0.1:3031", "https://relay.example", community)
            .expect("valid binding");
        assert_eq!(binding.websocket_url(), "ws://127.0.0.1:3031");
        assert_eq!(binding.http_url(), "http://127.0.0.1:3031");
        assert_eq!(binding.canonical_origin(), "wss://relay.example");
        assert!(
            EdgeBinding::new("wss://127.0.0.1:3031", "https://relay.example", community).is_none()
        );
        assert!(EdgeBinding::new(
            "ws://relay.example:3031",
            "https://relay.example",
            community
        )
        .is_none());
        assert!(EdgeBinding::new(
            "ws://127.0.0.1:3031/path",
            "https://relay.example",
            community
        )
        .is_none());
    }

    #[test]
    fn only_kind_nine_uuid_channel_filters_are_edge_eligible() {
        let channel = Uuid::new_v4();
        let h = SingleLetterTag::lowercase(Alphabet::H);
        let message = Filter::new()
            .kind(Kind::Custom(MESSAGE_KIND))
            .custom_tags(h, [channel.to_string()]);
        assert!(filters_are_message_only(std::slice::from_ref(&message)));
        assert!(!filters_are_message_only(&[Filter::new()
            .kind(Kind::Custom(39000))
            .custom_tags(h, [channel.to_string()])]));
        assert!(!filters_are_message_only(&[
            Filter::new().kind(Kind::Custom(MESSAGE_KIND))
        ]));
    }
}
