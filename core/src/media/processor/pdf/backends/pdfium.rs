//! The PDFium backend.
//!
//! This is the historical rendering pipeline and still the default one. PDFium is a C++ library,
//! so it must be loaded from a shared library at runtime (see [`renderer`]).

use std::{
	collections::HashMap,
	path::Path,
	sync::{Mutex, MutexGuard, OnceLock},
};

use image::RgbaImage;
use pdfium_render::prelude::{
	PdfColor, PdfDocument, PdfDocumentMetadataTagType, PdfPageIndex, PdfRenderConfig,
	Pdfium,
};

use crate::{
	config::StumpConfig,
	media::processor::{error::MediaProcessorError, pdf::backends::target_dimensions},
	metadata::ProcessedMediaMetadata,
};

/// alright so tldr; this is a temporary workaround (the mutex) until upstream fixes issues with
/// the `thread_safe` feature that make it unsafe (lol).
/// im subscribed to the following and will remove once able after resolved:
/// - https://github.com/ajrcarey/pdfium-render/issues/262
static PDFIUM: OnceLock<Result<Mutex<Pdfium>, MediaProcessorError>> = OnceLock::new();

/// Returns a mutex-guarded pdfium singleton, initializing it on first call
///
/// Note that a failed init is cached permanently and requires a restart the process to retry
/// initialization
///
/// See: https://github.com/ajrcarey/pdfium-render#thread-safety
pub fn renderer(
	pdfium_path: &Option<String>,
) -> Result<MutexGuard<'static, Pdfium>, MediaProcessorError> {
	// TODO: revert to returning a good ol &'static Pdfium once upstream fixes
	// the issue outlined above
	let result = PDFIUM.get_or_init(|| {
		let path = pdfium_path
			.clone()
			.or_else(|| std::env::var("PDFIUM_PATH").ok());

		if let Some(path) = path {
			tracing::info!(path, "Initializing PDFium from provided path");
			let bindings = Pdfium::bind_to_library(&path)
				.or_else(|e| {
					tracing::error!(provided_path = ?path, ?e, "Failed to bind to PDFium library at provided path, attempting fallback to system library");
					Pdfium::bind_to_system_library()
				})
				.map_err(|e| {
					tracing::error!(?e, "Failed to bind to system PDFium library");
					MediaProcessorError::PdfConfiguration
				})?;
			Ok(Mutex::new(Pdfium::new(bindings)))
		} else {
			tracing::warn!(
				"No PDFium path provided, will attempt to bind to system library"
			);
			Pdfium::bind_to_system_library()
				.map(|b| Mutex::new(Pdfium::new(b)))
				.map_err(|e| {
					tracing::error!(?e, "Failed to bind to system PDFium library");
					MediaProcessorError::PdfConfiguration
				})
		}
	});

	result
		.as_ref()
		.map_err(|_| MediaProcessorError::PdfConfiguration)?
		.lock()
		.map_err(|_| {
			tracing::error!(
				"PDFium mutex is poisoned, you will need to restart the process to recover from this error"
			);
			MediaProcessorError::PdfConfiguration
		})
}

pub fn is_available(config: &StumpConfig) -> bool {
	renderer(&config.pdfium_path).is_ok()
}

pub fn page_count(path: &Path, config: &StumpConfig) -> Result<i32, MediaProcessorError> {
	let pdfium = renderer(&config.pdfium_path)?;
	let document = pdfium.load_pdf_from_file(path.to_str().unwrap_or_default(), None)?;
	Ok(document.pages().len())
}

pub fn page_size(
	path: &Path,
	page: i32,
	config: &StumpConfig,
) -> Result<(f32, f32), MediaProcessorError> {
	let pdfium = renderer(&config.pdfium_path)?;
	let document = pdfium.load_pdf_from_file(path.to_str().unwrap_or_default(), None)?;

	let total_pages = document.pages().len() as usize;
	if page < 1 || page as usize > total_pages {
		return Err(MediaProcessorError::PageNotFound);
	}

	let size = document.pages().page_size(((page - 1) as u16).into())?;
	Ok((size.width().value, size.height().value))
}

pub fn extract_metadata(
	path: &Path,
	config: &StumpConfig,
) -> Result<Option<ProcessedMediaMetadata>, MediaProcessorError> {
	let pdfium = renderer(&config.pdfium_path)?;
	let document = pdfium.load_pdf_from_file(path.to_str().unwrap_or_default(), None)?;
	extract_metadata_from_open_document(&document)
}

pub fn render_page(
	path: &Path,
	page: i32,
	config: &StumpConfig,
	force_high_quality: bool,
) -> Result<RgbaImage, MediaProcessorError> {
	let pdfium = renderer(&config.pdfium_path)?;
	let document = pdfium.load_pdf_from_file(path.to_str().unwrap_or_default(), None)?;
	let total_pages = document.pages().len() as usize;

	if page < 1 || page as usize > total_pages {
		return Err(MediaProcessorError::PageNotFound);
	}

	let page_index = (page - 1) as PdfPageIndex;
	let document_page = document.pages().get(page_index)?;

	let (target_width, target_height) = target_dimensions(
		document_page.width().value,
		document_page.height().value,
		config,
		force_high_quality,
	);

	let use_high_quality = force_high_quality || config.pdf_high_quality;
	let render_config = if use_high_quality {
		PdfRenderConfig::new()
			.set_target_width(target_width)
			.set_maximum_height(target_height)
			.use_print_quality(true)
			.set_image_smoothing(true)
			.set_text_smoothing(true)
			.set_path_smoothing(true)
			.set_clear_color(PdfColor::new(255, 255, 255, 255))
			.clear_before_rendering(true)
	} else {
		PdfRenderConfig::new()
			.set_target_width(target_width)
			.set_maximum_height(target_height)
			.use_print_quality(false)
			.set_image_smoothing(false)
			.set_text_smoothing(true)
			.set_path_smoothing(false)
			.set_clear_color(PdfColor::new(255, 255, 255, 255))
			.clear_before_rendering(true)
	};

	let bitmap = document_page.render_with_config(&render_config)?;
	let dyn_image = bitmap.as_image()?;

	Ok(dyn_image.to_rgba8())
}

/// Extract metadata from a PDF file provided an already-open PdfDocument
fn extract_metadata_from_open_document(
	document: &PdfDocument<'_>,
) -> Result<Option<ProcessedMediaMetadata>, MediaProcessorError> {
	let mut metadata_map: HashMap<String, Vec<String>> = HashMap::new();
	for tag in document.metadata().iter() {
		let value_ref = tag.value();
		if value_ref.is_empty() {
			continue;
		}
		let value = value_ref.to_string();
		let key = match tag.tag_type() {
			PdfDocumentMetadataTagType::Title => "title".to_string(),
			PdfDocumentMetadataTagType::Author => "author".to_string(),
			PdfDocumentMetadataTagType::Subject => "summary".to_string(),
			PdfDocumentMetadataTagType::Keywords => "tags".to_string(),
			PdfDocumentMetadataTagType::CreationDate => "date".to_string(),
			PdfDocumentMetadataTagType::ModificationDate => "date".to_string(),
			_ => continue,
		};
		metadata_map.entry(key).or_default().push(value);
	}

	Ok(if metadata_map.is_empty() {
		None
	} else {
		Some(ProcessedMediaMetadata::from(metadata_map))
	})
}
