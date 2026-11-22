//! PDF processing.
//!
//! PDFs are treated differently from the other media types: instead of converting the whole file
//! up front, pages are rendered on demand, cached on disk and optionally pre-rendered around the
//! page being read.
//!
//! The actual parsing and rasterization is delegated to one of the [`backends`], which can be
//! selected with the `pdf_backend` config option. Stump is migrating from PDFium (which needs a
//! shared library to be installed) to hayro (pure Rust); PDFium remains the default for now.

mod backends;

use std::{
	collections::HashMap,
	io::Cursor,
	path::{Path, PathBuf},
};

use models::shared::image_processor_options::SupportedImageFormat;

use crate::{
	config::StumpConfig,
	fs_utils::{
		ContentType, FileParts, PathUtils,
		archive::create_zip_archive,
		hash::{self, generate_koreader_hash},
	},
	image::into_image_format,
	media::processor::{
		AnalyzedPage, GeneratedFileHashes, MediaProcessor, MediaProcessorOptions,
		ProcessedMediaFile, error::MediaProcessorError,
	},
	metadata::ProcessedMediaMetadata,
};

pub(crate) use backends::PdfBackend;

pub struct PdfProcessor {
	config: StumpConfig,
}

impl PdfProcessor {
	pub fn new(config: StumpConfig) -> Self {
		Self { config }
	}
}

impl MediaProcessor for PdfProcessor {
	fn generate_stump_hash(&self, path: &Path) -> Result<String, MediaProcessorError> {
		let size = std::fs::metadata(path)?.len();
		if size < 10 {
			return Err(MediaProcessorError::FileTooSmall);
		}
		Ok(hash::generate(path, size / 10)?)
	}

	fn generate_hashes(
		&self,
		path: &Path,
		MediaProcessorOptions {
			generate_file_hashes,
			generate_koreader_hashes,
			..
		}: MediaProcessorOptions,
	) -> Result<GeneratedFileHashes, MediaProcessorError> {
		let stump_hash = generate_file_hashes
			.then(|| self.generate_stump_hash(path))
			.transpose()?;
		let koreader_hash = generate_koreader_hashes
			.then(|| generate_koreader_hash(path))
			.transpose()?;

		Ok(GeneratedFileHashes {
			stump: stump_hash,
			koreader: koreader_hash,
		})
	}

	fn process_metadata(
		&self,
		path: &Path,
	) -> Result<Option<ProcessedMediaMetadata>, MediaProcessorError> {
		process_metadata_internal(path, &self.config)
	}

	fn process(
		&self,
		path: &Path,
		options: MediaProcessorOptions,
	) -> Result<ProcessedMediaFile, MediaProcessorError> {
		let backend = PdfBackend::from_config(&self.config);
		let pages = backend.page_count(path, &self.config)?;
		let metadata = if options.process_metadata {
			backend.extract_metadata(path, &self.config)?
		} else {
			None
		};

		let hashes = self.generate_hashes(path, options)?;

		Ok(ProcessedMediaFile {
			path: path.to_path_buf(),
			hash: hashes.stump,
			koreader_hash: hashes.koreader,
			metadata,
			pages,
		})
	}

	fn get_page(
		&self,
		path: &Path,
		page: i32,
	) -> Result<(ContentType, Vec<u8>), MediaProcessorError> {
		render_page_with_quality(path, page, &self.config, false)
	}

	fn get_page_count(&self, path: &Path) -> Result<i32, MediaProcessorError> {
		PdfBackend::from_config(&self.config).page_count(path, &self.config)
	}

	fn get_page_content_types(
		&self,
		_: &Path,
		pages: Vec<i32>,
	) -> Result<HashMap<i32, ContentType>, MediaProcessorError> {
		let content_type = ContentType::from(self.config.get_pdf_render_format());
		Ok(pages.into_iter().map(|page| (page, content_type)).collect())
	}

	fn analyze_page(
		&self,
		path: &Path,
		page: i32,
	) -> Result<AnalyzedPage, MediaProcessorError> {
		let backend = PdfBackend::from_config(&self.config);
		let (width, height) = backend.page_size(path, page, &self.config)?;

		let output_format = self.config.get_pdf_render_format();
		let content_type = ContentType::from(output_format);

		Ok(AnalyzedPage {
			width: width as u32,
			height: height as u32,
			content_type,
		})
	}
}

/// Process the metadata of a PDF file using the configured backend.
pub fn process_metadata_internal(
	path: &Path,
	config: &StumpConfig,
) -> Result<Option<ProcessedMediaMetadata>, MediaProcessorError> {
	PdfBackend::from_config(config).extract_metadata(path, config)
}

/// Render a single page with the configured backend and encode it in the configured format.
pub fn render_page_with_quality(
	path: &Path,
	page: i32,
	config: &StumpConfig,
	force_high_quality: bool,
) -> Result<(ContentType, Vec<u8>), MediaProcessorError> {
	let backend = PdfBackend::from_config(config);
	tracing::debug!(
		backend = backend.as_str(),
		path = %path.display(),
		page,
		force_high_quality,
		"Rendering PDF page"
	);
	let image = backend.render_page(path, page, config, force_high_quality)?;

	let output_format = config.get_pdf_render_format();
	let image_format = into_image_format(output_format);
	let content_type = ContentType::from(output_format);

	let mut buffer = Cursor::new(vec![]);
	image.write_to(&mut buffer, image_format)?;
	Ok((content_type, buffer.into_inner()))
}

/// Async version of get_page with caching support
pub async fn get_page_async(
	path: &Path,
	page: i32,
	config: &StumpConfig,
) -> Result<(ContentType, Vec<u8>), MediaProcessorError> {
	if config.pdf_cache_pages {
		if let Ok(Some(cached)) = get_cached_page(path, page, config).await {
			if config.pdf_prerender_range > 0 {
				let path_owned = path.to_path_buf();
				let config_owned = config.clone();
				tokio::spawn(async move {
					prerender_adjacent_pages(&path_owned, page, &config_owned).await;
				});
			}
			return Ok(cached);
		}
	}

	let path_owned = path.to_path_buf();
	let config_owned = config.clone();

	let result = tokio::task::spawn_blocking(move || {
		render_page_with_quality(&path_owned, page, &config_owned, false)
	})
	.await??;

	if config.pdf_cache_pages {
		// non-fatal, does not rly matter if caching fails
		let _ = cache_page(path, page, &result.1, config).await;
	}

	if config.pdf_cache_pages && config.pdf_prerender_range > 0 {
		let path_owned = path.to_path_buf();
		let config_owned = config.clone();
		tokio::spawn(async move {
			prerender_adjacent_pages(&path_owned, page, &config_owned).await;
		});
	}

	Ok(result)
}

// TODO: part of the reorg and realized this was never actually used, use it or remove it
#[allow(dead_code)]
pub fn convert_to_zip(
	path: &Path,
	delete_source: bool,
	format: Option<SupportedImageFormat>,
	config: &StumpConfig,
) -> Result<PathBuf, MediaProcessorError> {
	// The format override is applied through the config so that the shared render helper can be
	// used for every page.
	let config = match format {
		Some(format) => StumpConfig {
			pdf_render_format: format.extension().to_string(),
			..config.clone()
		},
		None => config.clone(),
	};

	let output_extension = config.get_pdf_render_format().extension();
	let total_pages = PdfBackend::from_config(&config).page_count(path, &config)?;

	let converted_pages = (1..=total_pages)
		.filter_map(
			|page| match render_page_with_quality(path, page, &config, true) {
				Ok((_, buf)) => Some(((page - 1) as usize, buf)),
				Err(e) => {
					tracing::error!(error = ?e, "Failed to render PDF page {}", page);
					None
				},
			},
		)
		.collect::<Vec<(usize, Vec<u8>)>>();

	let FileParts {
		file_name,
		file_stem,
		extension,
	} = path.file_parts();
	let parent = path.parent().unwrap_or_else(|| Path::new("/"));

	let cache_dir = config.cache_directory();
	let unpacked_path = cache_dir.join(&file_stem);

	std::fs::create_dir_all(&unpacked_path)?;

	for (idx, image_buf) in converted_pages {
		let image_path = unpacked_path.join(format!(
			"{}_{:03}.{}",
			file_stem,
			idx + 1,
			output_extension
		));
		if let Err(err) = std::fs::write(&image_path, image_buf) {
			// non-fatal, don't think we should fail the entire conversion if one page fails to write
			tracing::error!(error = ?err, ?image_path, "Failed to write PDF page image to file");
		}
	}

	let zip_path = create_zip_archive(&unpacked_path, &file_name, &extension, parent)?;

	if delete_source {
		// note that the trash::delete won't work in docker, but or_else should hit in that case
		let _ = trash::delete(path).or_else(|_| std::fs::remove_file(path));
	}

	// TODO: guard against accidentally deleting important paths
	if unpacked_path.exists() {
		let _ = std::fs::remove_dir_all(&unpacked_path);
	}

	Ok(zip_path)
}

fn generate_cache_key(
	pdf_path: &Path,
	page: i32,
	config: &StumpConfig,
) -> Result<String, MediaProcessorError> {
	let metadata = std::fs::metadata(pdf_path)?;
	let modified_time = metadata
		.modified()?
		.duration_since(std::time::UNIX_EPOCH)
		.map_err(|e| MediaProcessorError::Unknown(e.to_string()))?
		.as_secs();
	let file_size = metadata.len();

	use std::collections::hash_map::DefaultHasher;
	use std::hash::{Hash, Hasher};

	let mut hasher = DefaultHasher::new();
	pdf_path.hash(&mut hasher);
	file_size.hash(&mut hasher);
	modified_time.hash(&mut hasher);
	// the backend is part of the key so that switching backends does not serve stale renders
	config.pdf_backend.hash(&mut hasher);
	config.pdf_max_dimension.hash(&mut hasher);
	config.pdf_render_dpi.hash(&mut hasher);
	config.pdf_render_format.hash(&mut hasher);
	config.pdf_high_quality.hash(&mut hasher);
	page.hash(&mut hasher);

	Ok(format!("pdf_{}_{}", hasher.finish(), page))
}

/// Check if a cached page exists and return its content
async fn get_cached_page(
	pdf_path: &Path,
	page: i32,
	config: &StumpConfig,
) -> Result<Option<(ContentType, Vec<u8>)>, MediaProcessorError> {
	if !config.pdf_cache_pages {
		return Ok(None);
	}

	let cache_key = generate_cache_key(pdf_path, page, config)?;
	let output_format = config.get_pdf_render_format();
	let cache_file = config.pdf_cache_directory().join(format!(
		"{}.{}",
		cache_key,
		output_format.extension()
	));

	if cache_file.exists() {
		match tokio::fs::read(&cache_file).await {
			Ok(bytes) => {
				if !bytes.is_empty() {
					return Ok(Some((ContentType::from(output_format), bytes)));
				}
				let _ = tokio::fs::remove_file(&cache_file).await;
			},
			Err(e) => {
				tracing::warn!(cache_file = ?cache_file, error = ?e, "Failed to read cache file");
			},
		}
	}

	Ok(None)
}

/// Save a rendered page to the cache
async fn cache_page(
	pdf_path: &Path,
	page: i32,
	content: &[u8],
	config: &StumpConfig,
) -> Result<(), MediaProcessorError> {
	if !config.pdf_cache_pages || content.is_empty() {
		return Ok(());
	}

	let cache_key = generate_cache_key(pdf_path, page, config)?;
	let cache_dir = config.pdf_cache_directory();

	if !cache_dir.exists() {
		if let Err(e) = tokio::fs::create_dir_all(&cache_dir).await {
			tracing::warn!(cache_dir = ?cache_dir, error = ?e, "Failed to create cache directory");
			return Ok(());
		}
	}

	let output_format = config.get_pdf_render_format();
	let cache_file =
		cache_dir.join(format!("{}.{}", cache_key, output_format.extension()));
	let temp_file =
		cache_dir.join(format!("{}.{}.tmp", cache_key, output_format.extension()));

	match tokio::fs::write(&temp_file, content).await {
		Ok(_) => match tokio::fs::rename(&temp_file, &cache_file).await {
			Ok(_) => {
				tracing::debug!(
					?cache_file,
					size = content.len(),
					"Cached PDF page successfully"
				);
			},
			Err(error) => {
				tracing::warn!(?cache_file, ?error, "Failed to move temp cache file");
				let _ = tokio::fs::remove_file(&temp_file).await;
			},
		},
		Err(error) => {
			tracing::warn!(?cache_file, ?error, "Failed to write to temp cache file");
		},
	}

	Ok(())
}

async fn prerender_adjacent_pages(
	pdf_path: &Path,
	current_page: i32,
	config: &StumpConfig,
) {
	if !config.pdf_cache_pages || config.pdf_prerender_range == 0 {
		return;
	}

	let total_pages = match PdfBackend::from_config(config).page_count(pdf_path, config) {
		Ok(count) => count,
		Err(e) => {
			tracing::debug!(
				pdf_path = %pdf_path.display(),
				error = ?e,
				"Failed to get page count for pre-rendering, skipping"
			);
			return;
		},
	};

	let range = config.pdf_prerender_range as i32;
	let start_page = (current_page - range).max(1);
	let end_page = (current_page + range).min(total_pages);

	tracing::debug!(
		pdf_path = %pdf_path.display(),
		current_page,
		start_page,
		end_page,
		total_pages,
		"Pre-rendering adjacent pages"
	);

	// sort closest-first so the most useful pages render first
	let mut pages_to_render: Vec<i32> = (start_page..=end_page)
		.filter(|&page| page != current_page)
		.collect();
	pages_to_render.sort_by_key(|&page| (page - current_page).abs());

	// TODO: with the hayro backend the pages could be rendered in parallel, since it does not
	// have the same thread-safety constraints as PDFium.
	for page in pages_to_render {
		if let Ok(Some(_)) = get_cached_page(pdf_path, page, config).await {
			continue; // already cached = no work
		}

		let path_clone = pdf_path.to_path_buf();
		let config_clone = config.clone();
		let render_result = tokio::task::spawn_blocking(move || {
			render_page_with_quality(&path_clone, page, &config_clone, true)
		})
		.await;

		match render_result {
			Ok(Ok((_, content))) => {
				if let Err(e) = cache_page(pdf_path, page, &content, config).await {
					tracing::debug!(
						pdf_path = %pdf_path.display(),
						page,
						error = ?e,
						"Failed to cache pre-rendered page"
					);
				} else {
					tracing::debug!(
						pdf_path = %pdf_path.display(),
						page,
						"Pre-rendered and cached page successfully"
					);
				}
			},
			Ok(Err(e)) => {
				tracing::debug!(
					pdf_path = %pdf_path.display(),
					page,
					error = ?e,
					"Failed to render page during pre-rendering"
				);
			},
			Err(e) => {
				tracing::debug!(
					pdf_path = %pdf_path.display(),
					page,
					error = ?e,
					"Pre-rendering task panicked"
				);
			},
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::media::fixtures::get_test_pdf_path;

	type TestBackend = (PdfBackend, StumpConfig);

	/// Every backend which can run in this environment, paired with a config enabling it.
	fn test_backends() -> Vec<TestBackend> {
		let base = StumpConfig::debug();
		let mut backends = Vec::new();

		if PdfBackend::Pdfium.is_available(&base) {
			backends.push((
				PdfBackend::Pdfium,
				StumpConfig {
					pdf_backend: "pdfium".to_string(),
					..base.clone()
				},
			));
		} else {
			eprintln!("Skipping PDFium: PDFium is not configured or available.");
		}

		backends.push((
			PdfBackend::Hayro,
			StumpConfig {
				pdf_backend: "hayro".to_string(),
				..base
			},
		));

		backends
	}

	#[test]
	fn test_process() {
		let path = get_test_pdf_path();
		for (backend, config) in test_backends() {
			let processor = PdfProcessor::new(config);
			let processed = processor.process(
				Path::new(&path),
				MediaProcessorOptions {
					process_metadata: true,
					..Default::default()
				},
			);

			assert!(processed.is_ok(), "{backend:?}: {processed:?}");
			assert!(processed.unwrap().pages > 0, "{backend:?}");
		}
	}

	#[test]
	fn test_process_metadata() {
		let path = get_test_pdf_path();
		for (backend, config) in test_backends() {
			let metadata = process_metadata_internal(Path::new(&path), &config);
			assert!(metadata.is_ok(), "{backend:?}: {metadata:?}");
		}
	}

	#[test]
	fn test_analyze_page() {
		let path = get_test_pdf_path();
		for (backend, config) in test_backends() {
			let processor = PdfProcessor::new(config);

			let page = processor.analyze_page(Path::new(&path), 1);
			assert!(page.is_ok(), "{backend:?}: {page:?}");

			let page = page.unwrap();
			assert!(page.width > 0, "{backend:?}");
			assert!(page.height > 0, "{backend:?}");
		}
	}

	#[test]
	fn test_get_page_content_types() {
		let path = get_test_pdf_path();
		let config = StumpConfig::debug();
		let processor = PdfProcessor::new(config);

		let content_types = processor.get_page_content_types(Path::new(&path), vec![1]);
		assert!(content_types.is_ok());
	}

	#[test]
	fn test_generate_stump_hash() {
		let path = get_test_pdf_path();
		let config = StumpConfig::debug();
		let processor = PdfProcessor::new(config);

		let hash = processor.generate_stump_hash(Path::new(&path));
		assert!(hash.is_ok());
	}

	#[test]
	fn test_generate_hashes() {
		let path = get_test_pdf_path();
		let config = StumpConfig::debug();
		let processor = PdfProcessor::new(config);

		let hashes = processor.generate_hashes(
			Path::new(&path),
			MediaProcessorOptions {
				generate_file_hashes: true,
				generate_koreader_hashes: true,
				..Default::default()
			},
		);
		assert!(hashes.is_ok());
		let hashes = hashes.unwrap();
		assert!(hashes.stump.is_some());
		assert!(hashes.koreader.is_some());
	}

	#[tokio::test]
	async fn test_get_page_async() {
		let path = get_test_pdf_path();
		for (backend, config) in test_backends() {
			// Test without cache
			let mut no_cache_config = config.clone();
			no_cache_config.pdf_cache_pages = false;
			let result = get_page_async(Path::new(&path), 1, &no_cache_config).await;
			assert!(result.is_ok(), "{backend:?}: {result:?}");

			// Test with cache enabled
			let mut cache_config = config.clone();
			let temp_dir = tempfile::tempdir().unwrap();
			cache_config.config_dir = temp_dir.path().to_string_lossy().to_string();

			let result1 = get_page_async(Path::new(&path), 1, &cache_config).await;
			assert!(result1.is_ok(), "{backend:?}: {result1:?}");

			// Second call should hit cache and yield identical output
			let result2 = get_page_async(Path::new(&path), 1, &cache_config).await;
			assert!(result2.is_ok(), "{backend:?}: {result2:?}");
			assert_eq!(result1.unwrap().1, result2.unwrap().1, "{backend:?}");
		}
	}

	#[tokio::test]
	async fn test_prerender_adjacent_pages() {
		let path = get_test_pdf_path();
		for (backend, mut config) in test_backends() {
			config.pdf_cache_pages = true;
			config.pdf_prerender_range = 1;
			let temp_dir = tempfile::tempdir().unwrap();
			config.config_dir = temp_dir.path().to_string_lossy().to_string();

			// This will pre-render page 2 in the background when requesting page 1
			let result = get_page_async(Path::new(&path), 1, &config).await;
			assert!(result.is_ok(), "{backend:?}: {result:?}");

			// Poll to prevent timing flakiness under load
			let cache_key = generate_cache_key(Path::new(&path), 2, &config).unwrap();
			let output_format = config.get_pdf_render_format();
			let cache_file = config.pdf_cache_directory().join(format!(
				"{}.{}",
				cache_key,
				output_format.extension()
			));

			let mut success = false;
			for _ in 0..100 {
				if cache_file.exists()
					&& cache_file.metadata().map(|m| m.len() > 0).unwrap_or(false)
				{
					success = true;
					break;
				}
				tokio::time::sleep(std::time::Duration::from_millis(100)).await;
			}
			assert!(success, "{backend:?}: page 2 was not pre-rendered");
		}
	}

	#[test]
	fn test_convert_to_zip() {
		// `tall.pdf` only has a single page, which keeps the test fast
		let path = crate::media::fixtures::test_data_dir().join("tall.pdf");
		for (backend, mut config) in test_backends() {
			let temp_dir = tempfile::tempdir().unwrap();
			config.config_dir = temp_dir.path().to_string_lossy().to_string();

			let result = convert_to_zip(&path, false, None, &config);
			assert!(result.is_ok(), "{backend:?}: {result:?}");

			let zip_path = result.unwrap();
			assert!(zip_path.exists(), "{backend:?}");
		}
	}

	#[test]
	fn test_hayro_renders_page() {
		let path = get_test_pdf_path();
		let config = StumpConfig {
			pdf_backend: "hayro".to_string(),
			..StumpConfig::debug()
		};
		let processor = PdfProcessor::new(config.clone());

		let analyzed = processor.analyze_page(Path::new(&path), 1).unwrap();
		let (content_type, bytes) =
			render_page_with_quality(Path::new(&path), 1, &config, true).unwrap();

		assert_eq!(content_type.mime_type(), "image/webp");
		let image = image::load_from_memory(&bytes).unwrap();
		assert!(image.width() > 0 && image.height() > 0);

		// the page is scaled to the configured maximum dimension while keeping its aspect ratio
		let max_dimension = config.pdf_max_dimension;
		assert!(image.width().max(image.height()) <= max_dimension);
		let expected_width = (image.height() as f32 * analyzed.width as f32
			/ analyzed.height as f32)
			.round() as u32;
		assert!(image.width().abs_diff(expected_width) <= 2);
	}

	#[test]
	fn test_hayro_out_of_bounds_page() {
		let path = get_test_pdf_path();
		let config = StumpConfig {
			pdf_backend: "hayro".to_string(),
			..StumpConfig::debug()
		};
		let processor = PdfProcessor::new(config);

		let page = processor.analyze_page(Path::new(&path), i32::MAX);
		assert!(matches!(page, Err(MediaProcessorError::PageNotFound)));
	}

	#[test]
	fn test_backends_agree() {
		let path = get_test_pdf_path();
		let base = StumpConfig::debug();
		if !PdfBackend::Pdfium.is_available(&base) {
			eprintln!(
				"Skipping backend parity check: PDFium is not configured or available."
			);
			return;
		}

		let pdfium_config = StumpConfig {
			pdf_backend: "pdfium".to_string(),
			..base.clone()
		};
		let hayro_config = StumpConfig {
			pdf_backend: "hayro".to_string(),
			..base
		};

		let pdfium_pages =
			PdfBackend::Pdfium.page_count(Path::new(&path), &pdfium_config);
		let hayro_pages = PdfBackend::Hayro.page_count(Path::new(&path), &hayro_config);
		assert_eq!(pdfium_pages.unwrap(), hayro_pages.unwrap());

		let pdfium_size =
			PdfBackend::Pdfium.page_size(Path::new(&path), 1, &pdfium_config);
		let hayro_size = PdfBackend::Hayro.page_size(Path::new(&path), 1, &hayro_config);
		let (pdfium_width, pdfium_height) = pdfium_size.unwrap();
		let (hayro_width, hayro_height) = hayro_size.unwrap();
		assert!((pdfium_width - hayro_width).abs() < 1.0);
		assert!((pdfium_height - hayro_height).abs() < 1.0);

		let (_, pdfium_bytes) =
			render_page_with_quality(Path::new(&path), 1, &pdfium_config, true).unwrap();
		let (_, hayro_bytes) =
			render_page_with_quality(Path::new(&path), 1, &hayro_config, true).unwrap();
		let pdfium_image = image::load_from_memory(&pdfium_bytes).unwrap();
		let hayro_image = image::load_from_memory(&hayro_bytes).unwrap();
		// both backends should produce pages of (roughly) the same size
		let width_diff = pdfium_image.width().abs_diff(hayro_image.width());
		let height_diff = pdfium_image.height().abs_diff(hayro_image.height());
		assert!(width_diff <= 2, "width differs by {width_diff}px");
		assert!(height_diff <= 2, "height differs by {height_diff}px");
	}
}
