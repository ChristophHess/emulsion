use std;
use std::io::Read;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;

use gelatin::glium;
use gelatin::image::{self, gif::GifDecoder, io::Reader, ImageFormat, AnimationDecoder};

use glium::texture::{MipmapsOption, RawImage2d, SrgbTexture2d};

pub mod errors {
	use gelatin::glium::texture;
	use gelatin::image;
	use std::io;

	error_chain! {
		foreign_links {
			Io(io::Error) #[doc = "Error during IO"];
			TextureCreationError(texture::TextureCreationError);
			ImageLoadError(image::ImageError);
		}
	}
}

use self::errors::*;

pub fn load_image(image_path: &Path) -> Result<image::RgbaImage> {
	Ok(image::open(image_path)?.to_rgba())
}

pub fn texture_from_image(
	display: &glium::Display,
	image: image::RgbaImage,
) -> Result<SrgbTexture2d> {
	let image_dimensions = image.dimensions();
	let image_data = image.into_raw();
	let raw_image = || RawImage2d::from_raw_rgba(image_data.clone(), image_dimensions);

	let x_pow = (31 as u32) - image_dimensions.0.leading_zeros();
	let y_pow = (31 as u32) - image_dimensions.1.leading_zeros();

	let max_mipmap_levels = x_pow.min(y_pow).min(4);

	let mipmaps = if max_mipmap_levels == 1 {
		MipmapsOption::NoMipmap
	} else {
		MipmapsOption::AutoGeneratedMipmapsMax(max_mipmap_levels)
	};

	Ok(SrgbTexture2d::with_mipmaps(display, raw_image(), mipmaps)?)
}

pub fn is_file_supported(filename: &Path) -> bool {
	if let Some(ext) = filename.extension() {
		if let Some(ext) = ext.to_str() {
			let ext = ext.to_lowercase();
			match ext.as_str() {
				"jpg" | "jpeg" | "png" | "gif" | "webp" | "tif" | "tiff" | "tga" | "bmp"
				| "ico" | "hdr" | "pbm" | "pam" | "ppm" | "pgm" => {
					return true;
				}
				_ => (),
			}
		}
	}
	false
}

pub struct LoadRequest {
	pub req_id: u32,
	pub path: PathBuf,
}

pub enum LoadResult {
	Start { req_id: u32, metadata: fs::Metadata },
	Frame { req_id: u32, image: image::RgbaImage, delay_nano: u64 },
	Done { req_id: u32 },
	Failed { req_id: u32 },
}

impl LoadResult {
	pub fn req_id(&self) -> u32 {
		match self {
			LoadResult::Start { req_id, .. } => *req_id,
			LoadResult::Frame { req_id, .. } => *req_id,
			LoadResult::Done { req_id, .. } => *req_id,
			LoadResult::Failed { req_id, .. } => *req_id,
		}
	}
	pub fn is_failed(&self) -> bool {
		if let LoadResult::Failed { .. } = *self {
			true
		} else {
			false
		}
	}
}

pub struct ImageLoader {
	running: Arc<AtomicBool>,
	join_handles: Option<Vec<thread::JoinHandle<()>>>,
	image_rx: Receiver<LoadResult>,
	path_tx: Sender<LoadRequest>,
}

impl ImageLoader {
	/// # Arguemnts
	/// * `capacity` - Number of bytes. The last image loaded will be the one at which the allocated memory reaches or exceeds capacity
	pub fn new(threads: u32) -> ImageLoader {
		let running = Arc::new(AtomicBool::from(true));
		//let loader_cache = HashMap::new();

		let (load_request_tx, load_request_rx) = channel();
		let load_request_rx = Arc::new(Mutex::new(load_request_rx));

		let (loaded_img_tx, loaded_img_rx) = channel();

		let mut join_handles = Vec::new();
		for _ in 0..threads {
			let running = running.clone();
			let load_request_rx = load_request_rx.clone();
			let loaded_img_tx = loaded_img_tx.clone();

			join_handles.push(thread::spawn(move || {
				Self::thread_loop(running, load_request_rx, loaded_img_tx);
			}));
		}

		ImageLoader {
			//curr_dir: PathBuf::new(),
			//curr_est_size: capacity as usize,
			running,
			//remaining_capacity: capacity,
			//total_capacity: capacity,
			//loader_cache,
			//texture_cache: BTreeMap::new(),
			join_handles: Some(join_handles),

			image_rx: loaded_img_rx,
			path_tx: load_request_tx,
			//requested_images: 0,
		}
	}

	fn thread_loop(
		running: Arc<AtomicBool>,
		load_request_rx: Arc<Mutex<Receiver<LoadRequest>>>,
		loaded_img_tx: Sender<LoadResult>,
	) {
		// The size was an arbitrary choice made with the argument that this should be
		// enough to fit enough image file info to determine the format.
		let mut file_start_bytes = [0; 512]; 
		while running.load(Ordering::Acquire) {
			let request = {
				let load_request = load_request_rx.lock().unwrap();
				load_request.recv().unwrap()
			};
			let mut load_succeeded = false;
			// It is very important that we release the mutex before starting to load the image
			if let Ok(metadata) = fs::metadata(&request.path) {
				let mut is_gif = false;
				if let Ok(mut file) = fs::File::open(&request.path) {
					if file.read_exact(&mut file_start_bytes).is_ok() {
						if let Ok(ImageFormat::Gif) = image::guess_format(&file_start_bytes) {
							is_gif = true;
						}
					}
				}
				loaded_img_tx.send(LoadResult::Start { req_id: request.req_id, metadata }).unwrap();
				if is_gif {
					if let Ok(file) = fs::File::open(&request.path) {
						if let Ok(decoder) = GifDecoder::new(file) {
							let frames = decoder.into_frames();
							load_succeeded = true;
							for frame in frames {
								if let Ok(frame) = frame {
									let (numerator_ms, denom_ms) = frame.delay().numer_denom_ms();
									let numerator_nano = numerator_ms as u64 * 1_000_000;
									let denom_nano = denom_ms as u64 * 1_000_000;
									let delay_nano = numerator_nano / denom_nano;
									let image = frame.into_buffer();
									loaded_img_tx
										.send(LoadResult::Frame { req_id: request.req_id, image, delay_nano })
										.unwrap();
								} else {
									load_succeeded = false;
									break;
								}
							}
						}
					}
				} else {
					if let Ok(image) = load_image(request.path.as_path()) {
						loaded_img_tx
							.send(LoadResult::Frame { req_id: request.req_id, image, delay_nano: 0 })
							.unwrap();
						loaded_img_tx.send(LoadResult::Done { req_id: request.req_id }).unwrap();
						load_succeeded = true;
					}
				}
			}
			if !load_succeeded {
				loaded_img_tx.send(LoadResult::Failed { req_id: request.req_id }).unwrap();
			}
		}
	}

	pub fn try_recv_prefetched(&mut self) -> std::result::Result<LoadResult, TryRecvError> {
		self.image_rx.try_recv()
	}

	pub fn send_load_request(&mut self, request: LoadRequest) {
		self.path_tx.send(request).unwrap();
	}
}

impl Drop for ImageLoader {
	fn drop(&mut self) {
		self.running.store(false, Ordering::Release);
		if let Some(join_handles) = self.join_handles.take() {
			for _ in join_handles.iter() {
				self.path_tx.send(LoadRequest { req_id: 0, path: PathBuf::from("") }).unwrap();
			}

			for handle in join_handles.into_iter() {
				if let Err(err) = handle.join() {
					eprintln!("Error occured while joining handle {:?}", err);
				}
			}
		}
	}
}
