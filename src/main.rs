#[macro_use]
extern crate failure;

extern crate chrono;

extern crate clap;

#[macro_use]
extern crate log;

extern crate fern;

#[macro_use]
extern crate serde_derive;

extern crate toml;

extern crate ctrlc;

extern crate regex;

extern crate bincode;

extern crate nix;

use std::fs::{OpenOptions, DirBuilder};
use std::io::{BufReader, ErrorKind as IoErrorKind, Result as IoResult};
use std::io::prelude::*;
use std::iter::FusedIterator;
use std::path::Path;
use std::process::{ChildStderr, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, Builder as ThreadBuilder, JoinHandle};

use failure::Error;

use clap::{App, Arg};

use regex::Regex;

#[derive(Debug, Deserialize)]
struct OptionConfig {
    /// The radius in blocks that will be pregenerated.
    pub radius: Option<u32>,

    /// The number of megabytes to allocate to the server.
    pub memory: Option<u32>,

    /// Used to configure the path of several resources required by lazy-pregen,
    /// including the Java command and the server JAR.
    pub resources: Option<OptionResources>,
}

#[derive(Debug, Deserialize)]
struct OptionResources {
    /// The command used to invoke Java.
    pub java: Option<String>,

    /// A path, either absolute or relative to the directory of this config file,
    /// where the server JAR is located.
    pub server: Option<String>,
}

/// The available configuration options in LazyPregen.toml.
#[derive(Debug, Serialize)]
pub struct Config {
    /// The radius in blocks that will be pregenerated.
    pub radius: u32,

    /// The number of megabytes to allocate to the server.
    pub memory: u32,

    /// Used to configure the path of several resources required by lazy-pregen,
    /// including the Java command and the server JAR.
    pub resources: Resources,
}

/// A set of configuration options that change the commands invoked or files
/// referenced in arguments passed to them.
#[derive(Debug, Serialize)]
pub struct Resources {
    /// The command used to invoke Java.
    pub java: String,

    /// A path, either absolute or relative to the directory of this config file,
    /// where the server JAR is located.
    pub server: String,
}

/// A set of booleans indicating whether the corresponding dimension will be pregenerated.
#[derive(Debug)]
pub struct Dimensions {
    pub overworld: bool,
    pub nether: bool,
    pub end: bool,
}

impl Config {
    /// This will practically never be reached by the pregenerator so is just a
    /// technicality. In future versions (if ever made) it might be nice to add
    /// a `PRACTICAL_RADIUS` so that a warning can be emitted if a configured
    /// radius value exceeds one that could be done in a reasonable amount of time.
    const MAX_RADIUS: u32 = 29999984;
}

impl Default for Config {
    #[inline]
    fn default() -> Config {
        Config {
            radius: 5000,
            memory: 2048,
            resources: Default::default(),
        }
    }
}

impl Default for Resources {
    #[inline]
    fn default() -> Resources {
        Resources {
            java: String::from("java"),
            server: String::from("./server.jar"),
        }
    }
}

#[derive(Debug)]
struct TextStream {
    receiver: Receiver<Text>,
    stdout_join: JoinHandle<()>,
    stderr_join: JoinHandle<()>,
}

impl TextStream {
    /// Creates a new `TextStream`, which merges a child standard output and error
    /// asynchronously (by using two other threads).
    #[inline]
    pub fn new(stdout_source: Receiver<ChildStdout>, stderr_source: Receiver<ChildStderr>) -> IoResult<TextStream> {
        // Create a receiver so each line can be received, then a sender for each thread.
        let (stdout_sender, receiver) = mpsc::channel();
        let stderr_sender = stdout_sender.clone();

        // Create the two threads.
        let stdout_join = ThreadBuilder::new().spawn(Self::remote_reader(stdout_source, Text::Stdout, stdout_sender))?;
        let stderr_join = ThreadBuilder::new().spawn(Self::remote_reader(stderr_source, Text::Stderr, stderr_sender))?;

        Ok(TextStream {
            receiver,
            stdout_join,
            stderr_join
        })
    }

    /// Create a closure that when spawned as a thread will send lines obtained from the reader to
    /// the receiving thread.
    #[inline]
    fn remote_reader<R, F>(reader_source: Receiver<R>, mut make_text: F, text_output: Sender<Text>) -> impl FnOnce() -> ()
    where
        R: Read,
        F: FnMut(String) -> Text + Send + Sync,
    {
        move || {
            match reader_source.recv() {
                Ok(reader) => {
                    let buf_read = BufReader::new(reader);
                    match buf_read.lines().try_for_each(move |line| {
                        if let Ok(line) = line {
                            text_output.send(make_text(line)).map_err(|_| ())
                        } else {
                            Err(())
                        }
                    }) {
                        Ok(()) => (),
                        Err(()) => {
                            if let Some(name) = thread::current().name() {
                                error!("IO error reading from stream {}.", name);
                            } else {
                                error!("IO error reading from a stream.");
                            }
                        }
                    }
                },
                // The sender(s) for this receiver are dropped. Thread execution cancelled.
                Err(_) => (),
            }
        }
    }
}

impl Iterator for TextStream {
    type Item = Text;

    fn next(&mut self) -> Option<Text> {
        match self.receiver.recv() {
            Ok(text) => Some(text),
            Err(_) => None,
        }
    }
}

impl FusedIterator for TextStream {}

#[derive(Clone, Debug)]
enum Text {
    Stdout(String),
    Stderr(String),
}

/// A cacheable structure holding the "ring" and an index to that "ring". It
/// can easily be converted into 2D X and Y coordinates (which would be used
/// as X and Z coordinates for a chunk in Minecraft).
///
/// This representation is used because it's easier and the word "lazy" is in
/// the name of the crate. It would probably be faster to just do calculations
/// on real X and Y coordinates.
///
/// # Illustration
///
/// ```text
/// Ring 0:
///    -X +X
/// +Z  3  2
/// -Z  0  1
///
/// Ring 1:
///    -X       +X
/// +Z  9  8  7  6
///    10  .  .  5
///    11  .  .  4
/// -Z  0  1  2  3
/// ```
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct PregenState {
    /// The radius of chunks generated so far.
    ring: u32,

    /// The index within the ring of chunks.
    index: u32,
}

impl PregenState {
    /// Create a new `PregenState`. Opts not to check ring and index, use `is_valid`
    /// for that.
    #[inline]
    pub fn new(ring: u32, index: u32) -> PregenState {
        PregenState {
            ring,
            index,
        }
    }

    /// Returns the ring stored in this state.
    #[inline]
    pub fn ring(self) -> u32 {
        self.ring
    }

    /// Returns the index stored in this state.
    #[inline]
    pub fn index(self) -> u32 {
        self.index
    }

    /// Increment the state to point to the next chunk.
    ///
    /// Returns whether the incrementation resulted in a ring change.
    #[inline]
    pub fn increment(&mut self) -> bool {
        let next_index = self.index + 1;

        if next_index == self.num_values() {
            self.ring = self.ring + 1;
            self.index = 0;
            true
        } else {
            self.index = next_index;
            false
        }
    }

    /// Calculate the X and Y (in 2D terms) of the chunk based on the internal state.
    #[inline]
    pub fn xy(self) -> (u32, u32) {
        let PregenState { ring, index } = self;

        // A "ring" is just the radius in chunks minus one. Therefore, this value is
        // equal to the diameter. A "side" of a ring is 2 shorter than a "base" (i.e.
        // top or bottom) because for calculation purposes we consider the corners to be
        // part of the top/bottom in all cases.
        let base_length = (ring + 1) * 2;

        // base_length * 2 - 2 is a simplification of:
        // (assume base_length)
        // let side_length = base_length - 2
        // let corner_length = base_length + side_length
        // corner_length
        if let Some(index) = index.checked_sub(base_length * 2 - 2) {
            // Index is either on the +Y edge or the -X edge.
            // And, now it is within a range such that we can use similar
            // calculations on it as with the -Y and +X edges.
            if let Some(index) = index.checked_sub(base_length) {
                // Index is on the -X edge.

                // -X edge implies X = 0.
                //
                // For Y, `base_length - 2 - index` is a simplification of:
                // (assume base_length and index)
                // let zero_indexed_side_max = base_length - 1
                // let side_without_corner_max = zero_indexed_side_max - 1
                // let y_index = side_without_corner_max - index
                // y_index
                (0, base_length - 2 - index)
            } else {
                // Index is on the +Y edge.

                // For X, `base_length - 1 - index` is a simplification of:
                // (assume base_length and index)
                // let zero_indexed_side_max = base_length - 1
                // let x_index = zero_indexed_side_max - index
                // x_index
                //
                // +Y edge implies Y = maximum Y.
                (base_length - 1 - index, base_length - 1)
            }
        } else {
            // Index is either on the -Y edge or the +X edge.
            if let Some(index) = index.checked_sub(base_length) {
                // Index is on the +X edge.

                // +X edge implies X = maximum X.
                //
                // For Y, `index + 1` is a simplification of:
                // (assume index)
                // let zero_indexed_side_min = 0
                // let side_without_corner_min = zero_indexed_side_min + 1
                // let y_index = side_without_corner_min + index
                // y_index
                (base_length - 1, index + 1)
            } else {
                // Index is on the -Y edge.

                // For X, `index` is a simplification of:
                // (assume index)
                // let zero_indexed_side_min = 0
                // let x_index = zero_indexed_side_min + index
                // x_index
                //
                // -Y edge implies Y = 0
                (index, 0)
            }
        }
    }

    /// Check whether the ring and index are valid. The only time they will be
    /// invalid is right after deserialization or creation.
    #[inline]
    pub fn is_valid(self) -> bool {
        self.index < self.num_values()
    }

    /// Retrieve the number of possible values in the current ring.
    #[inline]
    pub fn num_values(self) -> u32 {
        self.ring * 8 + 4
    }
}

#[derive(Clone, Copy, Debug)]
struct PregenController {
    state: PregenState,
    max_ring: u32,
}

impl PregenController {
    /// Create a new `PregenController` with a potentially cached state.
    /// This is infallible usually, but an invalid state (if the LazyPregen.lock
    /// file was corrupted) will cause an error to be returned.
    ///
    /// The radius is in blocks and is rounded up to a chunk boundary and converted
    /// internally to a chunk count.
    ///
    /// Returns None if the radius is 0 or if it is clear from the state that the
    /// ring was already generated.
    #[inline]
    pub fn new(radius: u32, state: Option<PregenState>) -> Result<Option<PregenController>, Error> {
        if let Some(state) = state {
            if !state.is_valid() {
                Err(format_err!("corrupt pregeneration state"))
            } else {
                if radius == 0 {
                    Ok(None)
                } else {
                    let max_ring = (radius - 1) / 16;

                    if state.ring() > max_ring || (state.ring() == max_ring && state.index() == state.num_values()) {
                        // No work to do. We've already generated this ring.
                        Ok(None)
                    } else {
                        Ok(Some(PregenController {
                            state,
                            max_ring,
                        }))
                    }
                }
            }
        } else {
            if radius == 0 {
                Ok(None)
            } else {
                let max_ring = (radius - 1) / 16;

                Ok(Some(PregenController {
                    state: PregenState::new(0, 0),
                    max_ring,
                }))
            }
        }
    }


    /// Run this function to issue commands to the server so long as "predicate"
    /// evaluates as true. Passes a radius and number of chunks to generate for
    /// each new ring to "output" for logging purposes. Passes a state to "cache"
    /// occasionally (after a successful `save-all flush`) for caching purposes.
    pub fn generate_chunks<W, P, O, C>(self, dimensions: Dimensions, mut console: TextStream, mut writer: W, mut predicate: P, mut output: O, mut cache: C) -> Result<(), Error>
    where
        W: Write,
        P: FnMut() -> bool,
        O: FnMut(u32, u32),
        C: FnMut(PregenState),
    {
        let PregenController { mut state, max_ring } = self;

        let mut count = 0;

        let save_success = Regex::new(r": Saved the game$").unwrap();

        let mut last_ring = ::std::u32::MAX;

        loop {
            if !predicate() {
                info!("Stopping pregeneration due to interrupt signal.");

                writer.write_all("save-all flush\n".as_bytes())?;
                writer.write_all("stop\n".as_bytes())?;

                cache(state.clone());
                break Ok(());
            }

            if state.ring() > max_ring {
                writer.write_all("save-all flush\n".as_bytes())?;
                writer.write_all("stop\n".as_bytes())?;

                // Return early (Err) is good, not return early (Ok) is bad.
                match console.try_for_each(|text| {
                    match text {
                        Text::Stdout(text) => if save_success.is_match(&text) {
                            Err(())
                        } else {
                            Ok(())
                        },
                        Text::Stderr(_) => Ok(()),
                    }
                }) {
                    Err(()) => {
                        cache(state.clone());

                        info!("Successfully generated chunks in a radius of {}.", state.ring() * 16);

                        break Ok(());
                    },
                    Ok(()) => break Err(format_err!("server ended after attempting to save world")),
                }
            } else if state.ring() != last_ring {
                last_ring = state.ring();

                output(last_ring * 16, state.num_values() - state.index());
            }

            let (chunk_x, chunk_z) = {
                let (x, y) = state.xy();
                let ring = state.ring();

                (
                    (x as i32) - (
                        (ring + 1)
                        as i32
                    ),
                    (y as i32) - (
                        (ring + 1)
                        as i32
                    ),
                )
            };

            let raw_x = chunk_x * 16 + 8;
            let raw_z = chunk_z * 16 + 8;

            if dimensions.overworld {
                write!(writer, "execute in overworld run forceload add {x} {z}\n", x = raw_x, z = raw_z)?;
            }

            if dimensions.nether {
                write!(writer, "execute in the_nether run forceload add {x} {z}\n", x = raw_x, z = raw_z)?;
            }

            if dimensions.end {
                write!(writer, "execute in the_end run forceload add {x} {z}\n", x = raw_x, z = raw_z)?;
            }

            state.increment();

            count += 1;

            if count == 512 {
                count = 0;

                writer.write_all("execute in overworld run forceload remove all\n".as_bytes())?;
                writer.write_all("execute in the_nether run forceload remove all\n".as_bytes())?;
                writer.write_all("execute in the_end run forceload remove all\n".as_bytes())?;
                writer.write_all("save-all flush\n".as_bytes())?;

                // Return early (Err) is good, not return early (Ok) is bad.
                match console.try_for_each(|text| {
                    match text {
                        Text::Stdout(text) => if save_success.is_match(&text) {
                            Err(())
                        } else {
                            Ok(())
                        },
                        Text::Stderr(_) => Ok(()),
                    }
                }) {
                    Err(()) => cache(state.clone()),
                    Ok(()) => break Err(format_err!("server ended after attempting to save world")),
                }
            }
        }
    }

    pub fn radius(&self) -> u32 {
        (self.max_ring + 1) * 16
    }
}

#[cfg(target_family = "unix")]
fn initialize_command(command: &mut Command) -> &mut Command {
    use std::io::Error as IoError;
    use std::os::unix::process::CommandExt;
    use nix::unistd::setsid;
    use nix::Error as NixError;

    unsafe {
        command.pre_exec(|| setsid().map_err(|err| {
            if let Some(errno) = err.as_errno() {
                IoError::from(errno)
            } else {
                let kind = match &err {
                    NixError::InvalidPath => IoErrorKind::NotFound,
                    NixError::InvalidUtf8 => IoErrorKind::InvalidData,
                    _ => IoErrorKind::Other,
                };
                IoError::new(kind, err)
            }
        }).map(|_| ()))
    }
}

#[cfg(target_os = "windows")]
fn initialize_command(command: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;

    command.creation_flags(0x00000200)
}

fn main() -> Result<(), Error> {
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        r.store(false, AtomicOrdering::SeqCst);
    }).expect("Error setting Ctrl-C handler");

    initialize_logging()?;

    let matches = App::new("lazy-pregen")
        .version(env!("CARGO_PKG_VERSION"))
        .author("alluet <alluet@alluet.com>")
        .arg(Arg::with_name("OVERWORLD")
            .long("overworld")
            .help("Restrict generation to the Overworld (and any other dimensions with provided flags)")
        )
        .arg(Arg::with_name("NETHER")
            .long("nether")
            .help("Restrict generation to the Nether (and any other dimensions with provided flags)")
        )
        .arg(Arg::with_name("END")
            .long("end")
            .help("Restrict generation to the End (and any other dimension with provided flags)")
        )
        .arg(Arg::with_name("FOLDER")
            .takes_value(true)
            .help("Sets the folder to run the pregenerator in. Default: cwd"))
        .get_matches();

    let dimensions = {
        let overworld = matches.is_present("OVERWORLD");
        let nether = matches.is_present("NETHER");
        let end = matches.is_present("END");

        if overworld || nether || end {
            Dimensions {
                overworld,
                nether,
                end,
            }
        } else {
            Dimensions {
                overworld: true,
                nether: true,
                end: true,
            }
        }
    };

    let path = matches
        .value_of_os("FOLDER")
        .map_or_else(
            // No Argument -> CWD
            || {
                info!("Commencing lazy-pregen in the current working directory.");
                Path::new("./")
            },
            // Argument -> Path
            |os_str| {
                info!("Commencing lazy-pregen in the following path: {}", os_str.to_string_lossy());
                Path::new(os_str)
            }
        );

    let config_path = path.join("LazyPregen.toml");

    // 1. Try opening config file, or log INFO that it's not there.
    //    a. In the case that it's not there, attempt to write a file with the
    //       default settings and log about it.
    //    b. Then log a bunch in case some of that stuff fails.
    // 2. Try reading from config file, or log ERROR that it cannot be done.
    // 3. Try parsing the config file, or log ERROR that it cannot be done.
    // 4. The parsed configuration (with default values filled in) in the case
    //    of success or the default configuration in the case of failure.
    let config = match OpenOptions::new()
        .read(true)
        .open(&config_path)
    {
        Ok(mut file) => {
            let mut raw_config = String::new();
            match file.read_to_string(&mut raw_config) {
                Ok(_) => {
                    toml::from_str(&raw_config)
                        .map(|option_config| {
                            let OptionConfig { radius, memory, resources } = option_config;

                            let mut config = Config::default();

                            if let Some(radius) = radius {
                                if radius > Config::MAX_RADIUS {
                                    error!("Configuration property \"radius\" with value {} is larger than the maximum ({}).", radius, Config::MAX_RADIUS);
                                    warn!("Falling back to the default value for \"radius\", {}.", config.radius);
                                } else {
                                    config.radius = radius;
                                }
                            } else {
                                info!("Configuration property \"radius\" unspecified. Using default: {}", config.radius);
                            }

                            if let Some(memory) = memory {
                                config.memory = memory;
                            } else {
                                info!("Configuration property \"memory\" unspecified. Using default: {}", config.memory);
                            }

                            if let Some(OptionResources { server, java }) = resources {
                                if let Some(server) = server {
                                    config.resources.server = server;
                                } else {
                                    info!("Configuration property \"resources.server\" unspecified. Using default: {}", &config.resources.server);
                                }

                                if let Some(java) = java {
                                    config.resources.java = java;
                                } else {
                                    info!("Configuration property \"resources.java\" unspecified. Using default: {}", &config.resources.java);
                                }
                            } else {
                                info!("Configuration property \"resources\" unspecified.");
                                info!("Using default for \"resources.server\": {}", &config.resources.server);
                                info!("Using default for \"resources.java\": {}", &config.resources.java);
                            }

                            config
                        })
                        .unwrap_or_else(|err| {
                            error!("Unable to parse configuration file (LazyPregen.toml): {}", err);
                            warn!("Falling back to default configuration. Please correct the configuration file (LazyPregen.toml).");

                            Config::default()
                        })
                },
                Err(err) => {
                    error!("Unable to read an opened configuration file (LazyPregen.toml): {}", err);
                    warn!("Falling back to default configuration.");

                    Config::default()
                },
            }
        },
        Err(err) => match err.kind() {
            IoErrorKind::NotFound => {
                info!("Configuration file (LazyPregen.toml) not found. Using default settings.");

                let config = Config::default();

                match DirBuilder::new()
                    .recursive(true)
                    .create(path)
                {
                    Ok(()) => {
                        match OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&config_path)
                        {
                            Ok(mut file) => {
                                let raw_config = toml::to_string(&config)?;
                                match file.write_all(raw_config.as_bytes()) {
                                    Ok(()) => info!("Default configuration file (LazyPregen.toml) written to disk."),
                                    Err(err) => warn!("Unable to initialize an opened configuration file (LazyPregen.toml) with default configuration: {}", err),
                                }
                            },
                            Err(err) => match err.kind() {
                                IoErrorKind::AlreadyExists => warn!("The recently created configuration file (LazyPregen.toml) will not be considered."),
                                _ => warn!("Unable to create a default configuration file (LazyPregen.toml): {}", err),
                            },
                        }
                    },
                    Err(err) => warn!("Unable to create directory for configuration file (LazyPregen.toml): {}", err),
                }

                config
            },
            _ => {
                error!("Unable to open configuration file (LazyPregen.toml): {}", err);
                warn!("Falling back to default configuration.");

                Config::default()
            }
        },
    };

    let lock_path = path.join("LazyPregen.lock");
    let mut overwrite_lock = true;

    let controller = {
        let state = match OpenOptions::new()
            .read(true)
            .open(&lock_path)
        {
            Ok(mut file) => {
                let state: Option<PregenState> = match bincode::deserialize_from(&mut file) {
                    Ok(state) => Some(state),
                    Err(_) => {
                        warn!("Error reading data from lock file (LazyPregen.lock).");
                        warn!("Saved progress will not be noted and future progress will not update the lock file.");
                        overwrite_lock = false;
                        None
                    }
                };
                state
            },
            Err(err) => {
                match err.kind() {
                    IoErrorKind::NotFound => (),
                    _ => {
                        warn!("Lock file (LazyPregen.lock) could not be read.");
                        warn!("Saved progress will not be noted and future progress will not update the lock file.");
                        overwrite_lock = false;
                    },
                }
                None
            }
        };

        match PregenController::new(config.radius, state) {
            Ok(Some(controller)) => {
                info!("Initializing pregeneration controller for radius {}.", controller.radius());
                controller
            },
            Ok(None) => {
                info!("For the given radius {}, no work needs to be done.", config.radius);
                info!("The radius is either zero or the LazyPregen.lock file indicates that the chunks are already generated.");
                info!("If you believe this is an error adjust the radius in LazyPregen.toml or delete LazyPregen.lock");
                return Ok(());
            },
            Err(err) => {
                warn!("Error creating pregeneration controller: {}", err);
                warn!("Creating pregeneration controller without taking previous progress into account.");
                warn!("Saved progress will not be noted and future progress will not update the lock file.");
                warn!("To resolve this, please delete the corrupt lock file (LazyPregen.lock).");

                match PregenController::new(config.radius, None) {
                    Ok(Some(controller)) => {
                        info!("Initializing pregeneration controller for radius {}.", controller.radius());
                        controller
                    },
                    Ok(None) => {
                        info!("For the given radius {}, no work needs to be done.", config.radius);
                        info!("The radius is either zero or the LazyPregen.lock file indicates that the chunks are already generated.");
                        info!("If you believe this is an error adjust the radius in LazyPregen.toml or delete LazyPregen.lock");
                        return Ok(());
                    },
                    Err(err) => unreachable!("error while creating pregeneration controller with safe arguments: {}", err),
                }
            }
        }
    };

    let xmx_arg = format!("-Xmx{}M", config.memory);

    let (send_stdout, recv_stdout) = mpsc::sync_channel::<ChildStdout>(1);
    let (send_stderr, recv_stderr) = mpsc::sync_channel::<ChildStderr>(1);

    if !running.load(AtomicOrdering::SeqCst) {
        info!("Quitting due to interrupt signal.");

        Ok(())
    } else {
        match TextStream::new(recv_stdout, recv_stderr) {
            Ok(mut stream) => match initialize_command(
                    Command::new(&config.resources.java)
                    .args([&xmx_arg, "-jar", &config.resources.server, "nogui"].into_iter())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                ).spawn()
            {
                Ok(mut child) => {
                    send_stdout.send(child.stdout.take().unwrap()).unwrap();
                    send_stderr.send(child.stderr.take().unwrap()).unwrap();
                    drop((send_stdout, send_stderr));

                    let re_start = Regex::new(r": Starting minecraft server version .*$").unwrap();

                    // We are only using try_for_each for its ability to return early while still
                    // providing the potential for speedup via internal iteration.
                    //
                    // Also, it provides the ability to differentiate between success and error.
                    // Success in this case is actually Err(()), because that means the console
                    // output indicating the server is starting was found.
                    match stream.try_for_each(|text| {
                        match text {
                            Text::Stdout(text) => if re_start.is_match(&text) {
                                Err(())
                            } else {
                                Ok(())
                            },
                            Text::Stderr(_) => Ok(()),
                        }
                    }) {
                        // Server is starting up.
                        Err(()) => {
                            let re_done = Regex::new(r#": Done (.+)! For help, type "help"$"#).unwrap();
                            let re_eula = Regex::new(r": You need to agree to the EULA in order to run the server\. Go to eula\.txt for more info\.$").unwrap();
                            let mut match_eula = false;
                            match stream.try_for_each(|text| {
                                match text {
                                    Text::Stdout(text) => if re_done.is_match(&text) {
                                        Err(())
                                    } else {
                                        if re_eula.is_match(&text) {
                                            match_eula = true;
                                        }
                                        Ok(())
                                    },
                                    Text::Stderr(_) => Ok(()),
                                }
                            }) {
                                // Server is successfully started up.
                                Err(()) => {
                                    // At this point we can send commands through the server's
                                    // standard input. The chunk generation will be through this.
                                    // We will write progress to LazyPregen.lock so that we do not
                                    // redo unnecessary work, but this is not a necessity and a
                                    // failure to write to that file will not cause the process to
                                    // end. A warning will be logged if writing fails after having
                                    // worked, but multiple failures in a row will not be logged.

                                    info!("Server started.");

                                    let (state_send, state_recv) = mpsc::sync_channel::<PregenState>(2);
                                    if overwrite_lock {
                                        if let Err(_) = ThreadBuilder::new().spawn(move || {
                                            let mut open_options = OpenOptions::new();
                                            open_options
                                                .write(true)
                                                .create(true);

                                            while let Ok(state) = state_recv.recv() {
                                                match open_options.open(&lock_path) {
                                                    Ok(file) => {
                                                        if let Err(err) = bincode::serialize_into(file, &state) {
                                                            error!("Unable to update lock file: {}", err);
                                                        }
                                                    },
                                                    Err(err) => {
                                                        error!("Unable to open lock file for writing: {}", err);
                                                    }
                                                }
                                            }
                                        }) {
                                            overwrite_lock = false;
                                        }
                                    }

                                    let (output_send, output_recv) = mpsc::sync_channel::<(u32, u32)>(2);
                                    let _ = ThreadBuilder::new().spawn(move || {
                                        while let Ok((radius, todo)) = output_recv.recv() {
                                            info!("Pregenerating {} chunks up to radius {}", todo, radius);
                                        }
                                    });

                                    if let Err(err) = controller.generate_chunks(
                                        // Which dimensions to generate chunks in
                                        dimensions,
                                        // Inspect output of console
                                        stream,
                                        // Write commands to console
                                        child.stdin.take().unwrap(),
                                        // Ctrl-C handler
                                        move || running.load(AtomicOrdering::SeqCst),
                                        // Log progress
                                        move |radius, todo| { let _ = output_send.send((radius, todo)); },
                                        // Cache state
                                        move |state| if overwrite_lock {
                                            if let Err(_) = state_send.send(state) {
                                                overwrite_lock = false;
                                            }
                                        },
                                    ) {
                                        error!("Chunk pregeneration failed: {}", err);

                                        child.wait().unwrap();

                                        Err(format_err!("fatal error"))
                                    } else {
                                        child.wait().unwrap();

                                        info!("Server stopped.");

                                        Ok(())
                                    }
                                },
                                // Server did not finish starting up.
                                Ok(()) => {
                                    if match_eula {
                                        error!("You need to indicate agreement to the Minecraft End User License Agreement in \"eula.txt\"");
                                        info!("Note that your use of the server through this program is still subject to your agreement with Mojang.");
                                    } else {
                                        error!("An error occured before the server is completely running. Read the server logs for more information.");
                                    }
                                    Err(format_err!("fatal error"))
                                },
                            }
                        },
                        // Server did not begin the start process.
                        Ok(()) => {
                            error!("An error occured before the server started up.");
                            Err(format_err!("fatal error"))
                        }
                    }
                },
                Err(err) => {
                    error!("The server could not be spawned: {}", err);
                    Err(format_err!("fatal error"))
                }
            },
            Err(err) => {
                error!("The standard output and error monitoring threads could not be spawned: {}", err);
                Err(format_err!("fatal error"))
            }
        }
    }
}

#[inline]
fn initialize_logging() -> Result<(), Error> {
    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "[{time}] {type:>5}: {message}",
                time = chrono::Local::now().format("%H:%M:%S"),
                type = format_level(record.level()),
                message = message,
            ));
        })
        .chain(std::io::stderr())
        .apply()?;

    Ok(())
}

#[inline]
fn format_level(level: log::Level) -> &'static str {
    use log::Level::*;
    match level {
        Error => "ERROR",
        Warn => " WARN",
        Info => " INFO",
        Debug => "DEBUG",
        Trace => "TRACE",
    }
}