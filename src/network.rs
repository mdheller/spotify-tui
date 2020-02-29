use crate::app::{ActiveBlock, App, RouteId, TrackTableContext};
use rspotify::{
    client::Spotify,
    model::{
        album::{FullAlbum, SavedAlbum, SimplifiedAlbum},
        artist::FullArtist,
        audio::AudioAnalysis,
        context::FullPlayingContext,
        device::DevicePayload,
        offset::for_position,
        page::{CursorBasedPage, Page},
        playing::PlayHistory,
        playlist::{PlaylistTrack, SimplifiedPlaylist},
        recommend::Recommendations,
        search::{SearchAlbums, SearchArtists, SearchPlaylists, SearchTracks},
        track::{FullTrack, SavedTrack, SimplifiedTrack},
        user::PrivateUser,
    },
    oauth2::{SpotifyClientCredentials, SpotifyOAuth, TokenInfo},
    senum::{Country, RepeatState},
    util::get_token,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tokio::try_join;

#[derive(Debug)]
pub enum IoEvent {
    GetCurrentPlayback,
    RefreshAuthentication,
    GetPlaylists,
    GetDevices,
    GetSearchResults(String, Option<Country>),
    SetTracksToTable(Vec<FullTrack>),
    GetMadeForYouPlaylistTracks(String, u32),
    GetPlaylistTracks(String, u32),
    GetCurrentSavedTracks(Option<u32>, bool),
}

pub fn get_spotify(token_info: TokenInfo) -> (Spotify, Instant) {
    let token_expiry = Instant::now()
        + Duration::from_secs(token_info.expires_in.into())
        // Set 10 seconds early
        - Duration::from_secs(10);

    let client_credential = SpotifyClientCredentials::default()
        .token_info(token_info)
        .build();

    let spotify = Spotify::default()
        .client_credentials_manager(client_credential)
        .build();

    (spotify, token_expiry)
}

pub struct Network {
    oauth: SpotifyOAuth,
    spotify: Spotify,
    spotify_token_expiry: Instant,
    // TODO: This needs to be updated from the main thread
    large_search_limit: u32,
    small_search_limit: u32,
}

type AppArc = Arc<Mutex<App>>;

impl Network {
    pub fn new(oauth: SpotifyOAuth, spotify: Spotify, spotify_token_expiry: Instant) -> Self {
        Network {
            oauth,
            spotify,
            spotify_token_expiry,
            large_search_limit: 20,
            small_search_limit: 4,
        }
    }

    pub async fn handle_network_event(&mut self, io_event: IoEvent, app: &AppArc) {
        match io_event {
            IoEvent::RefreshAuthentication => {
                if let Some(new_token_info) = get_token(&mut self.oauth).await {
                    let (new_spotify, new_token_expiry) = get_spotify(new_token_info);
                    self.spotify = new_spotify;
                    self.spotify_token_expiry = new_token_expiry;
                } else {
                    println!("\nFailed to refresh authentication token");
                    // TODO panic!
                }
            }
            IoEvent::GetPlaylists => {
                let playlists = self
                    .spotify
                    .current_user_playlists(self.large_search_limit, None)
                    .await;

                match playlists {
                    Ok(p) => {
                        let mut app = app.lock().await;
                        app.playlists = Some(p);
                        // Select the first playlist
                        app.selected_playlist_index = Some(0);
                    }
                    Err(e) => {
                        let mut app = app.lock().await;
                        app.handle_error(e);
                    }
                };

                self.get_user(&app).await;
            }
            IoEvent::GetDevices => {
                self.get_devices(&app).await;
            }
            IoEvent::GetCurrentPlayback => {
                self.get_current_playback(&app).await;
            }
            IoEvent::SetTracksToTable(full_tracks) => {
                self.set_tracks_to_table(&app, full_tracks).await;
            }
            IoEvent::GetSearchResults(search_term, country) => {
                self.get_search_results(&app, search_term, country).await;
            }
            IoEvent::GetMadeForYouPlaylistTracks(playlist_id, made_for_you_offset) => {
                self.get_made_for_you_playlist_tracks(&app, playlist_id, made_for_you_offset)
                    .await;
            }
            IoEvent::GetPlaylistTracks(playlist_id, playlist_offset) => {
                self.get_playlist_tracks(&app, playlist_id, playlist_offset)
                    .await;
            }
            IoEvent::GetCurrentSavedTracks(offset, should_navigate) => {
                self.get_current_user_saved_tracks(&app, offset, should_navigate)
                    .await;
            }
        };
    }

    pub async fn get_user(&self, app: &AppArc) {
        match self.spotify.current_user().await {
            Ok(user) => {
                let mut app = app.lock().await;
                app.user = Some(user);
            }
            Err(e) => {
                let mut app = app.lock().await;
                app.handle_error(e);
            }
        }
    }

    pub async fn get_devices(&self, app: &AppArc) {
        if let Ok(result) = self.spotify.device().await {
            let mut app = app.lock().await;
            app.push_navigation_stack(RouteId::SelectedDevice, ActiveBlock::SelectDevice);
            if !result.devices.is_empty() {
                app.devices = Some(result);
                // Select the first device in the list
                app.selected_device_index = Some(0);
            }
        }
    }

    pub async fn get_current_playback(&self, app: &AppArc) {
        let context = self.spotify.current_playback(None).await;
        if let Ok(ctx) = context {
            if let Some(c) = ctx {
                if let Some(track) = &c.item {
                    if let Some(track_id) = &track.id {
                        self.current_user_saved_tracks_contains(app, vec![track_id.to_owned()])
                            .await;
                    }
                }
                let mut app = app.lock().await;
                app.current_playback_context = Some(c.clone());
                app.instant_since_last_current_playback_poll = Instant::now();
            };
        }
    }

    pub async fn current_user_saved_tracks_contains(&self, app: &AppArc, ids: Vec<String>) {
        match self.spotify.current_user_saved_tracks_contains(&ids).await {
            Ok(is_saved_vec) => {
                for (i, id) in ids.iter().enumerate() {
                    if let Some(is_liked) = is_saved_vec.get(i) {
                        let mut app = app.lock().await;
                        if *is_liked {
                            app.liked_song_ids_set.insert(id.to_string());
                        } else {
                            // The song is not liked, so check if it should be removed
                            if app.liked_song_ids_set.contains(id) {
                                app.liked_song_ids_set.remove(id);
                            }
                        }
                    };
                }
            }
            Err(e) => {
                let mut app = app.lock().await;
                app.handle_error(e);
            }
        }
    }

    pub async fn get_playlist_tracks(
        &self,
        app: &AppArc,
        playlist_id: String,
        playlist_offset: u32,
    ) {
        if let Ok(playlist_tracks) = self
            .spotify
            .user_playlist_tracks(
                "spotify",
                &playlist_id,
                None,
                Some(self.large_search_limit),
                Some(playlist_offset),
                None,
            )
            .await
        {
            self.set_playlist_tracks_to_table(app, &playlist_tracks)
                .await;

            let mut app = app.lock().await;
            app.playlist_tracks = Some(playlist_tracks);
            if app.get_current_route().id != RouteId::TrackTable {
                app.push_navigation_stack(RouteId::TrackTable, ActiveBlock::TrackTable);
            };
        };
    }

    async fn set_playlist_tracks_to_table(
        &self,
        app: &AppArc,
        playlist_track_page: &Page<PlaylistTrack>,
    ) {
        self.set_tracks_to_table(
            app,
            playlist_track_page
                .items
                .clone()
                .into_iter()
                .map(|item| item.track.unwrap())
                .collect::<Vec<FullTrack>>(),
        )
        .await;
    }

    pub async fn set_tracks_to_table(&self, app: &AppArc, tracks: Vec<FullTrack>) {
        self.current_user_saved_tracks_contains(
            app,
            tracks
                .clone()
                .into_iter()
                .filter_map(|item| item.id)
                .collect::<Vec<String>>(),
        )
        .await;

        let mut app = app.lock().await;
        app.track_table.tracks = tracks;
    }

    pub async fn get_made_for_you_playlist_tracks(
        &self,
        app: &AppArc,
        playlist_id: String,
        made_for_you_offset: u32,
    ) {
        if let Ok(made_for_you_tracks) = self
            .spotify
            .user_playlist_tracks(
                "spotify",
                &playlist_id,
                None,
                Some(self.large_search_limit),
                Some(made_for_you_offset),
                None,
            )
            .await
        {
            self.set_playlist_tracks_to_table(app, &made_for_you_tracks)
                .await;

            let mut app = app.lock().await;
            app.made_for_you_tracks = Some(made_for_you_tracks);
            if app.get_current_route().id != RouteId::TrackTable {
                app.push_navigation_stack(RouteId::TrackTable, ActiveBlock::TrackTable);
            }
        }
    }

    pub async fn get_search_results(
        &self,
        app: &AppArc,
        search_term: String,
        country: Option<Country>,
    ) {
        let search_track =
            self.spotify
                .search_track(&search_term, self.small_search_limit, 0, country);

        let search_artist =
            self.spotify
                .search_artist(&search_term, self.small_search_limit, 0, country);

        let search_album =
            self.spotify
                .search_album(&search_term, self.small_search_limit, 0, country);

        let search_playlist =
            self.spotify
                .search_playlist(&search_term, self.small_search_limit, 0, country);

        // Run the futures concurrently
        match try_join!(search_track, search_artist, search_album, search_playlist) {
            Ok((track_results, artist_results, album_results, playlist_results)) => {
                self.set_tracks_to_table(app, track_results.tracks.items.clone())
                    .await;
                let mut app = app.lock().await;
                app.search_results.tracks = Some(track_results);
                app.search_results.artists = Some(artist_results);
                app.search_results.albums = Some(album_results);
                app.search_results.playlists = Some(playlist_results);
            }
            Err(e) => {
                let mut app = app.lock().await;
                app.handle_error(e);
            }
        };
    }

    pub async fn get_current_user_saved_tracks(
        &self,
        app: &AppArc,
        offset: Option<u32>,
        should_navigate: bool,
    ) {
        match self
            .spotify
            .current_user_saved_tracks(self.large_search_limit, offset)
            .await
        {
            Ok(saved_tracks) => {
                let mut app = app.lock().await;
                app.set_saved_tracks_to_table(&saved_tracks);

                app.library.saved_tracks.add_pages(saved_tracks);
                app.track_table.context = Some(TrackTableContext::SavedTracks);

                if should_navigate {
                    app.push_navigation_stack(RouteId::TrackTable, ActiveBlock::TrackTable);
                }
            }
            Err(e) => {
                let mut app = app.lock().await;
                app.handle_error(e);
            }
        }
    }
}
