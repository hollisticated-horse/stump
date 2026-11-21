//! The PDF engines which can be used to parse, inspect and rasterize PDF files.
//!
//! Stump is in the middle of migrating from [`pdfium`](https://pdfium.googlesource.com/pdfium/)
//! (via the `pdfium-render` crate, which requires a shared library to be installed) to
//! [`hayro`](https://github.com/LaurenzV/hayro), a pure-Rust PDF renderer. For now both engines
//! are supported and can be selected with the `pdf_backend` config option; the PDFium backend
//! remains the default until hayro has proven itself in the wild.
//!
//! Both backends are stateless: every operation opens the file itself. If/when PDFium is dropped,
//! this module can be collapsed into the hayro backend.

use std::path::Path;

use image::RgbaImage;

use crate::{
	config::StumpConfig, media::processor::error::MediaProcessorError,
	metadata::ProcessedMediaMetadata,
};

pub(crate) mod hayro;
pub(crate) mod pdfium;

/// The PDF engine used to parse and render PDF files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum PdfBackend {
	/// PDFium, loaded from a system or user-provided shared library.
	#[default]
	Pdfium,
	/// Hayro, a pure-Rust PDF renderer.
	Hayro,
}

impl PdfBackend {
	/// Resolve the configured backend, falling back to PDFium for unknown values.
	pub fn from_config(config: &StumpConfig) -> Self {
		let backend = match config.pdf_backend.to_lowercase().as_str() {
			"hayro" => Self::Hayro,
			"pdfium" => Self::Pdfium,
			other => {
				tracing::warn!(
					backend = other,
					"Unknown PDF backend configured, falling back to PDFium"
				);
				Self::Pdfium
			},
		};

		if !backend.is_available(config) {
			// Note: this is not fatal here. The actual error is surfaced by the render call, which
			// keeps the failure handling in one place.
			tracing::warn!(
				backend = backend.as_str(),
				"The configured PDF backend is not available"
			);
		}

		backend
	}

	pub fn as_str(self) -> &'static str {
		match self {
			Self::Pdfium => "pdfium",
			Self::Hayro => "hayro",
		}
	}

	/// Whether the backend can be used in this environment. PDFium needs a loadable shared
	/// library, while hayro is always available.
	pub fn is_available(self, config: &StumpConfig) -> bool {
		match self {
			Self::Pdfium => pdfium::is_available(config),
			Self::Hayro => true,
		}
	}

	pub fn page_count(
		self,
		path: &Path,
		config: &StumpConfig,
	) -> Result<i32, MediaProcessorError> {
		match self {
			Self::Pdfium => pdfium::page_count(path, config),
			Self::Hayro => hayro::page_count(path),
		}
	}

	/// The dimensions of the page in PDF points, accounting for rotation.
	pub fn page_size(
		self,
		path: &Path,
		page: i32,
		config: &StumpConfig,
	) -> Result<(f32, f32), MediaProcessorError> {
		match self {
			Self::Pdfium => pdfium::page_size(path, page, config),
			Self::Hayro => hayro::page_size(path, page),
		}
	}

	pub fn extract_metadata(
		self,
		path: &Path,
		config: &StumpConfig,
	) -> Result<Option<ProcessedMediaMetadata>, MediaProcessorError> {
		match self {
			Self::Pdfium => pdfium::extract_metadata(path, config),
			Self::Hayro => hayro::extract_metadata(path),
		}
	}

	/// Render a single page to an RGBA buffer. Page numbering is 1-based.
	pub fn render_page(
		self,
		path: &Path,
		page: i32,
		config: &StumpConfig,
		force_high_quality: bool,
	) -> Result<RgbaImage, MediaProcessorError> {
		match self {
			Self::Pdfium => pdfium::render_page(path, page, config, force_high_quality),
			Self::Hayro => hayro::render_page(path, page, config, force_high_quality),
		}
	}
}

/// Compute the target pixel dimensions for a page, given its size in PDF points.
///
/// This scales from the 72 DPI base to the configured DPI, clamps the result to the configured
/// maximum dimension and, for the "fast" mode, renders at 80% of that size.
pub(crate) fn target_dimensions(
	page_width: f32,
	page_height: f32,
	config: &StumpConfig,
	force_high_quality: bool,
) -> (i32, i32) {
	let dpi_scale = config.pdf_render_dpi as f32 / 72.0;
	let mut target_width = page_width * dpi_scale;
	let mut target_height = page_height * dpi_scale;

	let max_dim = config.pdf_max_dimension as f32;
	if max_dim > 0.0 && (target_width > max_dim || target_height > max_dim) {
		let fit_scale = (max_dim / target_width).min(max_dim / target_height);
		target_width *= fit_scale;
		target_height *= fit_scale;
	}

	let target_width = target_width.max(1.0) as i32;
	let target_height = target_height.max(1.0) as i32;

	if force_high_quality || config.pdf_high_quality {
		(target_width, target_height)
	} else {
		// Fast rendering while maintaining text readability
		(
			(target_width * 4 / 5).max(1),
			(target_height * 4 / 5).max(1),
		)
	}
}
