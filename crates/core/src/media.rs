use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::Bytes;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A binary payload that can come from different sources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BinarySource {
    /// A URL pointing to the binary resource.
    Url { url: String },

    /// Base64-encoded inline data with a media type.
    Base64 {
        media_type: String,
        #[serde(serialize_with = "serialize_bytes_as_base64")]
        #[serde(deserialize_with = "deserialize_bytes_from_base64")]
        data: Bytes,
    },

    /// Raw bytes held in memory (not serialized over the wire — use Base64 instead).
    #[serde(skip)]
    Bytes { media_type: String, data: Bytes },

    /// A provider-specific file or attachment ID.
    ProviderFileId { file_id: String },
}

fn serialize_bytes_as_base64<S: Serializer>(bytes: &Bytes, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&STANDARD.encode(bytes))
}

fn deserialize_bytes_from_base64<'de, D: Deserializer<'de>>(d: D) -> Result<Bytes, D::Error> {
    let s = String::deserialize(d)?;
    let decoded = STANDARD.decode(&s).map_err(serde::de::Error::custom)?;
    Ok(Bytes::from(decoded))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_variant_round_trips() {
        let src = BinarySource::Url {
            url: "https://example.com/img.png".into(),
        };
        let json = serde_json::to_string(&src).unwrap();
        let back: BinarySource = serde_json::from_str(&json).unwrap();
        assert_eq!(src, back);
    }

    #[test]
    fn base64_variant_round_trips() {
        let data = Bytes::from_static(b"\x89PNG\r\n");
        let src = BinarySource::Base64 {
            media_type: "image/png".into(),
            data,
        };
        let json = serde_json::to_string(&src).unwrap();
        assert!(json.contains("\"type\":\"base64\""));
        let back: BinarySource = serde_json::from_str(&json).unwrap();
        assert_eq!(src, back);
    }

    #[test]
    fn provider_file_id_round_trips() {
        let src = BinarySource::ProviderFileId {
            file_id: "file-abc123".into(),
        };
        let json = serde_json::to_string(&src).unwrap();
        let back: BinarySource = serde_json::from_str(&json).unwrap();
        assert_eq!(src, back);
    }
}
