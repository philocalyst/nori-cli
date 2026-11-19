//! ACP prompt blocks as SDK content.

use antigravity_sdk_rust::types::AudioMime;
use antigravity_sdk_rust::types::Content;
use antigravity_sdk_rust::types::ContentPrimitive;
use antigravity_sdk_rust::types::DocumentMime;
use antigravity_sdk_rust::types::ImageMime;
use antigravity_sdk_rust::types::Media;
use antigravity_sdk_rust::types::MimeType;
use antigravity_sdk_rust::types::VideoMime;
use nori_acp_kit::AgentError;
use nori_acp_kit::schema::ContentBlock;
use nori_acp_kit::schema::EmbeddedResourceResource;
use nori_acp_kit::schema::Role;

/// Convert ACP prompt content blocks into SDK [`Content`].
pub(crate) fn prompt_to_content(prompt: Vec<ContentBlock>) -> Result<Content, AgentError> {
    let mut parts = Vec::new();
    for block in prompt {
        match block {
            ContentBlock::Text(text) => {
                let audience = text
                    .annotations
                    .as_ref()
                    .and_then(|annotations| annotations.audience.as_ref());
                if audience.is_some_and(|audience| !audience.contains(&Role::Assistant)) {
                    continue;
                }
                parts.push(ContentPrimitive::Text(text.text));
            }
            ContentBlock::Image(image) => {
                if image.data.is_empty() {
                    if let Some(uri) = image.uri {
                        parts.push(ContentPrimitive::Text(format!("[image]({uri})")));
                    } else {
                        return Err(AgentError::invalid("Image has no data or URI"));
                    }
                } else if let Some(mime) = parse_mime(&image.mime_type)
                    && let Ok(bytes) = decode_base64(&image.data)
                {
                    parts.push(ContentPrimitive::Media(Media {
                        data: bytes,
                        mime_type: mime,
                        description: None,
                    }));
                } else {
                    parts.push(ContentPrimitive::Text(format!(
                        "[image: {}]",
                        image.mime_type
                    )));
                }
            }
            ContentBlock::Audio(audio) => {
                if let Some(mime) = parse_mime(&audio.mime_type)
                    && let Ok(bytes) = decode_base64(&audio.data)
                {
                    parts.push(ContentPrimitive::Media(Media {
                        data: bytes,
                        mime_type: mime,
                        description: None,
                    }));
                }
            }
            ContentBlock::Resource(resource) => match resource.resource {
                EmbeddedResourceResource::TextResourceContents(text) => {
                    parts.push(ContentPrimitive::Text(format!(
                        "[{}]\n{}",
                        text.uri, text.text
                    )));
                }
                EmbeddedResourceResource::BlobResourceContents(blob) => {
                    if let Some(mime_str) = blob.mime_type.as_deref()
                        && let Some(mime) = parse_mime(mime_str)
                        && let Ok(bytes) = decode_base64(&blob.blob)
                    {
                        parts.push(ContentPrimitive::Media(Media {
                            data: bytes,
                            mime_type: mime,
                            description: Some(blob.uri),
                        }));
                    } else {
                        parts.push(ContentPrimitive::Text(format!("[resource: {}]", blob.uri)));
                    }
                }
                _ => {
                    return Err(AgentError::invalid("Unsupported embedded resource"));
                }
            },
            ContentBlock::ResourceLink(link) => {
                parts.push(ContentPrimitive::Text(format!(
                    "[{}]({})",
                    link.name, link.uri
                )));
            }
            _ => {
                return Err(AgentError::invalid("Unsupported prompt content"));
            }
        }
    }

    if parts.is_empty() {
        return Err(AgentError::invalid(
            "Prompt contained no assistant-visible text",
        ));
    }
    if parts.len() == 1
        && let Some(single) = parts.pop()
    {
        return Ok(Content::Single(single));
    }
    Ok(Content::Multi(parts))
}

fn parse_mime(mime: &str) -> Option<MimeType> {
    match mime.to_ascii_lowercase().as_str() {
        "image/bmp" => Some(MimeType::Image(ImageMime::Bmp)),
        "image/jpeg" | "image/jpg" => Some(MimeType::Image(ImageMime::Jpeg)),
        "image/png" => Some(MimeType::Image(ImageMime::Png)),
        "image/webp" => Some(MimeType::Image(ImageMime::Webp)),
        "application/pdf" => Some(MimeType::Document(DocumentMime::Pdf)),
        "application/json" => Some(MimeType::Document(DocumentMime::Json)),
        "text/css" => Some(MimeType::Document(DocumentMime::Css)),
        "text/csv" => Some(MimeType::Document(DocumentMime::Csv)),
        "text/html" => Some(MimeType::Document(DocumentMime::Html)),
        "application/javascript" => Some(MimeType::Document(DocumentMime::Javascript)),
        "text/plain" => Some(MimeType::Document(DocumentMime::PlainText)),
        "text/rtf" => Some(MimeType::Document(DocumentMime::Rtf)),
        "application/xml" => Some(MimeType::Document(DocumentMime::Xml)),
        "audio/wav" => Some(MimeType::Audio(AudioMime::Wav)),
        "audio/mp3" => Some(MimeType::Audio(AudioMime::Mp3)),
        "audio/aac" => Some(MimeType::Audio(AudioMime::Aac)),
        "audio/ogg" => Some(MimeType::Audio(AudioMime::Ogg)),
        "audio/flac" => Some(MimeType::Audio(AudioMime::Flac)),
        "audio/opus" => Some(MimeType::Audio(AudioMime::Opus)),
        "audio/mpeg" => Some(MimeType::Audio(AudioMime::Mpeg)),
        "audio/m4a" => Some(MimeType::Audio(AudioMime::M4a)),
        "audio/l16" => Some(MimeType::Audio(AudioMime::L16)),
        "video/mp4" => Some(MimeType::Video(VideoMime::Mp4)),
        "video/webm" => Some(MimeType::Video(VideoMime::Webm)),
        _ => None,
    }
}

fn decode_base64(input: &str) -> Result<Vec<u8>, ()> {
    let mut out = Vec::with_capacity((input.len() / 4) * 3);
    let mut buf: u32 = 0;
    let mut bits: u8 = 0;
    for byte in input.bytes() {
        if byte.is_ascii_whitespace() || byte == b'=' {
            continue;
        }
        let val = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return Err(()),
        };
        buf = (buf << 6) | u32::from(val);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}
