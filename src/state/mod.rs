// Document data and the viewport state that reads it.
mod document;
mod document_imp;

pub(crate) use document::{
    preview_cache_budget, zoom_from_percent, zoom_is_supported, zoom_percent_text, State,
    PREVIEW_TARGET_BYTES,
};

#[cfg(test)]
pub(crate) use document::use_scratch_state_dir;
