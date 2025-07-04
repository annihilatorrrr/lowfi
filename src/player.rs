//! Responsible for playing & queueing audio.
//! This also has the code for the underlying
//! audio server which adds new tracks.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use arc_swap::ArcSwapOption;
use downloader::Downloader;
use reqwest::Client;
use rodio::{OutputStream, OutputStreamHandle, Sink};
use tokio::{
    select,
    sync::{
        mpsc::{Receiver, Sender},
        RwLock,
    },
    task,
};

#[cfg(feature = "mpris")]
use mpris_server::{PlaybackStatus, PlayerInterface, Property};

use crate::{
    messages::Messages,
    play::{PersistentVolume, SendableOutputStream},
    tracks::{self, list::List},
    Args,
};

pub mod audio;
pub mod bookmark;
pub mod downloader;
pub mod queue;
pub mod ui;

#[cfg(feature = "mpris")]
pub mod mpris;

/// The time to wait in between errors.
const TIMEOUT: Duration = Duration::from_secs(3);

/// Main struct responsible for queuing up & playing tracks.
// TODO: Consider refactoring [Player] from being stored in an [Arc], into containing many smaller [Arc]s.
// TODO: In other words, this would change the type from `Arc<Player>` to just `Player`.
// TODO:
// TODO: This is conflicting, since then it'd clone ~10 smaller [Arc]s
// TODO: every single time, which could be even worse than having an
// TODO: [Arc] of an [Arc] in some cases (Like with [Sink] & [Client]).
pub struct Player {
    /// [rodio]'s [`Sink`] which can control playback.
    pub sink: Sink,

    /// The internal buffer size.
    pub buffer_size: usize,

    /// Whether the current track has been bookmarked.
    bookmarked: AtomicBool,

    /// The [`TrackInfo`] of the current track.
    /// This is [`None`] when lowfi is buffering/loading.
    current: ArcSwapOption<tracks::Info>,

    /// The tracks, which is a [`VecDeque`] that holds
    /// *undecoded* [Track]s.
    ///
    /// This is populated specifically by the [Downloader].
    tracks: RwLock<VecDeque<tracks::QueuedTrack>>,

    /// The actual list of tracks to be played.
    list: List,

    /// The initial volume level.
    volume: PersistentVolume,

    /// The web client, which can contain a `UserAgent` & some
    /// settings that help lowfi work more effectively.
    client: Client,

    /// The [`OutputStreamHandle`], which also can control some
    /// playback, is for now unused and is here just to keep it
    /// alive so the playback can function properly.
    _handle: OutputStreamHandle,
}

impl Player {
    /// Just a shorthand for setting `current`.
    fn set_current(&self, info: tracks::Info) {
        self.current.store(Some(Arc::new(info)));
    }

    /// A shorthand for checking if `self.current` is [Some].
    pub fn current_exists(&self) -> bool {
        self.current.load().is_some()
    }

    /// Sets the volume of the sink, and also clamps the value to avoid negative/over 100% values.
    pub fn set_volume(&self, volume: f32) {
        self.sink.set_volume(volume.clamp(0.0, 1.0));
    }

    /// Initializes the entire player, including audio devices & sink.
    ///
    /// This also will load the track list & persistent volume.
    pub async fn new(args: &Args) -> eyre::Result<(Self, SendableOutputStream)> {
        // Load the volume file.
        let volume = PersistentVolume::load().await?;

        // Load the track list.
        let list = List::load(args.track_list.as_ref()).await?;

        // We should only shut up alsa forcefully on Linux if we really have to.
        #[cfg(target_os = "linux")]
        let (stream, handle) = if !args.alternate && !args.debug {
            audio::silent_get_output_stream()?
        } else {
            OutputStream::try_default()?
        };

        // If we're not on Linux, then there's no problem.
        #[cfg(not(target_os = "linux"))]
        let (stream, handle) = OutputStream::try_default()?;

        let sink = Sink::try_new(&handle)?;
        if args.paused {
            sink.pause();
        }

        let client = Client::builder()
            .user_agent(concat!(
                env!("CARGO_PKG_NAME"),
                "/",
                env!("CARGO_PKG_VERSION")
            ))
            .timeout(TIMEOUT)
            .build()?;

        let player = Self {
            tracks: RwLock::new(VecDeque::with_capacity(args.buffer_size)),
            buffer_size: args.buffer_size,
            current: ArcSwapOption::new(None),
            client,
            sink,
            volume,
            list,
            _handle: handle,
            bookmarked: AtomicBool::new(false),
        };

        Ok((player, SendableOutputStream(stream)))
    }

    /// This is the main "audio server".
    ///
    /// `rx` & `tx` are used to communicate with it, for example when to
    /// skip tracks or pause.
    ///
    /// This will also initialize a [Downloader] as well as an MPRIS server if enabled.
    /// The [Downloader]s internal buffer size is determined by `buf_size`.
    pub async fn play(
        player: Arc<Self>,
        tx: Sender<Messages>,
        mut rx: Receiver<Messages>,
        debug: bool,
    ) -> eyre::Result<()> {
        // Initialize the mpris player.
        //
        // We're initializing here, despite MPRIS being a "user interface",
        // since we need to be able to *actively* write new information to MPRIS
        // specifically when it occurs, unlike the UI which passively reads the
        // information each frame. Blame MPRIS, not me.
        #[cfg(feature = "mpris")]
        let mpris = mpris::Server::new(Arc::clone(&player), tx.clone())
            .await
            .inspect_err(|x| {
                dbg!(x);
            })?;

        // `itx` is used to notify the `Downloader` when it needs to download new tracks.
        let downloader = Downloader::new(Arc::clone(&player));
        let (itx, downloader) = downloader.start(debug);

        // Start buffering tracks immediately.
        Downloader::notify(&itx).await?;

        // Set the initial sink volume to the one specified.
        player.set_volume(player.volume.float());

        // Whether the last signal was a `NewSong`. This is helpful, since we
        // only want to autoplay if there hasn't been any manual intervention.
        //
        // In other words, this will be `true` after a new track has been fully
        // loaded and it'll be `false` if a track is still currently loading.
        let mut new = false;

        loop {
            let clone = Arc::clone(&player);

            let msg = select! {
                biased;

                Some(x) = rx.recv() => x,
                // This future will finish only at the end of the current track.
                // The condition is a kind-of hack which gets around the quirks
                // of `sleep_until_end`.
                //
                // That's because `sleep_until_end` will return instantly if the sink
                // is uninitialized. That's why we put a check to make sure that the last
                // signal we got was `NewSong`, since we shouldn't start waiting for the
                // song to be over until it has actually started.
                //
                // It's also important to note that the condition is only checked at the
                // beginning of the loop, not throughout.
                Ok(()) = task::spawn_blocking(move || clone.sink.sleep_until_end()),
                        if new => Messages::Next,
            };

            match msg {
                Messages::Next | Messages::Init | Messages::TryAgain => {
                    player.bookmarked.swap(false, Ordering::Relaxed);

                    // We manually skipped, so we shouldn't actually wait for the song
                    // to be over until we recieve the `NewSong` signal.
                    new = false;

                    // This basically just prevents `Next` while a song is still currently loading.
                    if msg == Messages::Next && !player.current_exists() {
                        continue;
                    }

                    // Handle the rest of the signal in the background,
                    // as to not block the main audio server thread.
                    task::spawn(Self::next(
                        Arc::clone(&player),
                        itx.clone(),
                        tx.clone(),
                        debug,
                    ));
                }
                Messages::Play => {
                    player.sink.play();

                    #[cfg(feature = "mpris")]
                    mpris.playback(PlaybackStatus::Playing).await?;
                }
                Messages::Pause => {
                    player.sink.pause();

                    #[cfg(feature = "mpris")]
                    mpris.playback(PlaybackStatus::Paused).await?;
                }
                Messages::PlayPause => {
                    if player.sink.is_paused() {
                        player.sink.play();
                    } else {
                        player.sink.pause();
                    }

                    #[cfg(feature = "mpris")]
                    mpris
                        .playback(mpris.player().playback_status().await?)
                        .await?;
                }
                Messages::ChangeVolume(change) => {
                    player.set_volume(player.sink.volume() + change);

                    #[cfg(feature = "mpris")]
                    mpris
                        .changed(vec![Property::Volume(player.sink.volume().into())])
                        .await?;
                }
                // This basically just continues, but more importantly, it'll re-evaluate
                // the select macro at the beginning of the loop.
                // See the top section to find out why this matters.
                Messages::NewSong => {
                    // We've recieved `NewSong`, so on the next loop iteration we'll
                    // begin waiting for the song to be over in order to autoplay.
                    new = true;

                    #[cfg(feature = "mpris")]
                    mpris
                        .changed(vec![
                            Property::Metadata(mpris.player().metadata().await?),
                            Property::PlaybackStatus(mpris.player().playback_status().await?),
                        ])
                        .await?;

                    continue;
                }
                Messages::Bookmark => {
                    let current = player.current.load();
                    let current = current.as_ref().unwrap();

                    let bookmarked = bookmark::bookmark(
                        current.full_path.clone(),
                        if current.custom_name {
                            Some(current.display_name.clone())
                        } else {
                            None
                        },
                    )
                    .await?;

                    player.bookmarked.swap(bookmarked, Ordering::Relaxed);
                }
                Messages::Quit => break,
            }
        }

        downloader.abort();

        Ok(())
    }
}
