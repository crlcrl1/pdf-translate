use anyhow::Result;
use lopdf::{Dictionary, Document, ObjectId};

/// Upper bound for a decoded content stream. Keeps malformed or malicious
/// compression bombs from allocating unbounded memory while still being far
/// larger than any normal page content stream.
pub const MAX_CONTENT_STREAM_BYTES: usize = 64 * 1024 * 1024;

pub(super) fn page_resources_clone(document: &Document, page_id: ObjectId) -> Result<Dictionary> {
    let (direct_resources, resource_refs) = document.get_page_resources(page_id)?;
    if let Some(resources) = direct_resources {
        return Ok(resources.clone());
    }
    if let Some(reference) = resource_refs.first() {
        return Ok(document.get_dictionary(*reference)?.clone());
    }
    Ok(Dictionary::new())
}

pub(super) fn page_original_content(
    document: &Document,
    page_id: ObjectId,
) -> Result<(Vec<u8>, Dictionary)> {
    let mut content = Vec::new();
    for stream_id in document.get_page_contents(page_id) {
        let stream = document.get_object(stream_id)?.as_stream()?;
        content.extend_from_slice(&stream.get_plain_content_with_limit(MAX_CONTENT_STREAM_BYTES)?);
        content.push(b'\n');
    }
    let resources = page_resources_clone(document, page_id)?;
    Ok((content, resources))
}
