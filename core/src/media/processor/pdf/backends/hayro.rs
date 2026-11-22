//! The hayro backend.
//!
//! [`hayro`](https://github.com/LaurenzV/hayro) is a pure-Rust PDF rasterizer, meaning it does
//! not require any shared library to be installed and it can render pages in parallel. It is
//! currently opt-in via `pdf_backend = "hayro"` until it has been validated against a wide range
//! of real-world PDFs.
//!
//! Only the top-level `hayro` crate is used. Its sibling crates (`hayro-svg`, `hayro-write`, ...)
//! are not needed to rasterize pages and are therefore not dependencies of Stump.

use std::{collections::HashMap, path::Path};

use hayro::{
	PixmapSettings, RenderCache, RenderSettings,
	hayro_interpret::InterpreterSettings,
	hayro_syntax::{Pdf, object::DateTime},
	render,
	vello_cpu::{color::palette::css::WHITE, peniko::ImageAlphaType},
};
use image::RgbaImage;

use crate::{
	config::StumpConfig,
	media::processor::{error::MediaProcessorError, pdf::backends::target_dimensions},
	metadata::ProcessedMediaMetadata,
};

/// A defensive upper bound for rendered pages.
///
/// `hayro` hands the output dimensions to `vello_cpu`, which stores them as `u16`. A very large
/// page (or an unlimited `pdf_max_dimension`) would otherwise allocate a multi-gigabyte pixmap.
/// The default configuration caps pages at [`StumpConfig::pdf_max_dimension`], so this is only
/// reached for pathological inputs.
const MAX_RENDER_DIMENSION: i32 = 8192;

fn load(path: &Path) -> Result<Pdf, MediaProcessorError> {
	let data = std::fs::read(path)?;
	Pdf::new(data).map_err(|e| MediaProcessorError::Hayro(format!("{e:?}")))
}

pub fn page_count(path: &Path) -> Result<i32, MediaProcessorError> {
	let pdf = load(path)?;
	Ok(pdf.pages().len() as i32)
}

pub fn page_size(path: &Path, page: i32) -> Result<(f32, f32), MediaProcessorError> {
	let pdf = load(path)?;
	let page = get_page(&pdf, page)?;
	Ok(page.render_dimensions())
}

pub fn extract_metadata(
	path: &Path,
) -> Result<Option<ProcessedMediaMetadata>, MediaProcessorError> {
	let pdf = load(path)?;
	let metadata = pdf.metadata();

	let mut metadata_map: HashMap<String, Vec<String>> = HashMap::new();
	let mut push = |key: &str, value: Option<String>| {
		if let Some(value) = value.filter(|value| !value.is_empty()) {
			metadata_map.entry(key.to_string()).or_default().push(value);
		}
	};

	push("title", metadata.title.as_deref().map(decode_text_string));
	push("author", metadata.author.as_deref().map(decode_text_string));
	push(
		"summary",
		metadata.subject.as_deref().map(decode_text_string),
	);
	push("tags", metadata.keywords.as_deref().map(decode_text_string));
	// Both dates land under the same key for parity with the PDFium backend, which also maps
	// both tags to `date`. Only the first value is ultimately used.
	push("date", metadata.creation_date.map(format_pdf_date));
	push("date", metadata.modification_date.map(format_pdf_date));

	Ok(if metadata_map.is_empty() {
		None
	} else {
		Some(ProcessedMediaMetadata::from(metadata_map))
	})
}

pub fn render_page(
	path: &Path,
	page: i32,
	config: &StumpConfig,
	force_high_quality: bool,
) -> Result<RgbaImage, MediaProcessorError> {
	let pdf = load(path)?;
	let document_page = get_page(&pdf, page)?;

	let (page_width, page_height) = document_page.render_dimensions();
	let (target_width, target_height) =
		target_dimensions(page_width, page_height, config, force_high_quality);

	// Both dimensions are capped by the same factor (like `PdfRenderConfig::set_target_width`
	// combined with `set_maximum_height`), so the page keeps its aspect ratio.
	let target_width = target_width.min(MAX_RENDER_DIMENSION);
	let target_height = target_height.min(MAX_RENDER_DIMENSION);
	let scale =
		(target_width as f32 / page_width).min(target_height as f32 / page_height);

	let high_quality = force_high_quality || config.pdf_high_quality;
	let render_settings = if high_quality {
		RenderSettings::default()
	} else {
		RenderSettings {
			hinting: false,
			..RenderSettings::default()
		}
	};

	// TODO: reuse this across pages (e.g. while pre-rendering) so that glyphs and outlines are
	// cached between renders.
	let cache = RenderCache::new();
	let pixmap = render(
		document_page,
		&cache,
		&InterpreterSettings::default(),
		&render_settings,
		&PixmapSettings {
			x_scale: scale,
			y_scale: scale,
			bg_color: WHITE,
		},
	);

	let width = pixmap.width() as u32;
	let height = pixmap.height() as u32;
	let rgba = pixmap.take_rgba8(ImageAlphaType::Alpha);

	RgbaImage::from_raw(width, height, rgba).ok_or(MediaProcessorError::PdfRenderFailed)
}

fn get_page<'a>(
	pdf: &'a Pdf,
	page: i32,
) -> Result<&'a hayro::hayro_syntax::page::Page<'a>, MediaProcessorError> {
	let total_pages = pdf.pages().len();
	if page < 1 || page as usize > total_pages {
		return Err(MediaProcessorError::PageNotFound);
	}

	pdf.pages()
		.get((page - 1) as usize)
		.ok_or(MediaProcessorError::PageNotFound)
}

/// Decode a PDF text string.
///
/// Text strings are either UTF-16BE (with a byte order mark), UTF-8 (with a byte order mark, PDF
/// 2.0) or PDFDocEncoding. PDFDocEncoding is treated as UTF-8 here, which is correct for the
/// ASCII range that covers the vast majority of real-world metadata.
fn decode_text_string(bytes: &[u8]) -> String {
	if let Some(bytes) = bytes.strip_prefix(&[0xFE, 0xFF]) {
		let units = bytes
			.chunks_exact(2)
			.map(|pair| u16::from_be_bytes([pair[0], pair[1]]));
		return String::from_utf16_lossy(&units.collect::<Vec<_>>());
	}

	let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
	String::from_utf8_lossy(bytes).into_owned()
}

/// Format a PDF date so that it can be parsed by the shared PDF date parser.
fn format_pdf_date(date: DateTime) -> String {
	format!(
		"D:{:04}{:02}{:02}{:02}{:02}{:02}",
		date.year, date.month, date.day, date.hour, date.minute, date.second
	)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn decode_utf8_text_string() {
		assert_eq!(decode_text_string(b"The Way of Kings"), "The Way of Kings");
		assert_eq!(decode_text_string(b""), "");
	}

	#[test]
	fn decode_utf16be_text_string() {
		// "Hi" in UTF-16BE with a byte order mark
		assert_eq!(
			decode_text_string(&[0xFE, 0xFF, 0x00, 0x48, 0x00, 0x69]),
			"Hi"
		);
	}

	#[test]
	fn format_pdf_date_is_parseable() {
		let date = DateTime {
			year: 2023,
			month: 4,
			day: 5,
			hour: 6,
			minute: 7,
			second: 8,
			utc_offset_hour: 0,
			utc_offset_minute: 0,
		};

		let formatted = format_pdf_date(date);
		assert_eq!(formatted, "D:20230405060708");
		assert_eq!(
			crate::metadata::utils::parse_pdf_date(&formatted),
			Some((2023, Some(4), Some(5)))
		);
	}
}
