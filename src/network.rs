use crate::app::{ActiveBlock, App, RouteId};
use rspotify::{
    client::Spotify,
    oauth2::{SpotifyClientCredentials, SpotifyOAuth, TokenInfo},
    util::get_token,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

#[derive(Debug)]
pub enum IoEvent {
    GetCurrentPlayback,
    RefreshAuthentication,
    GetPlaylists,
    GetDevices,
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

pub struct Network<'a> {
    oauth: &'a mut SpotifyOAuth,
    spotify: Spotify,
    spotify_token_expiry: Instant,
}

type AppArc = Arc<Mutex<App>>;

impl<'a> Network<'a> {
    pub fn new(
        oauth: &'a mut SpotifyOAuth,
        spotify: Spotify,
        spotify_token_expiry: Instant,
    ) -> Self {
        Network {
            oauth,
            spotify,
            spotify_token_expiry,
        }
    }

    pub async fn handle_network_event(&mut self, io_event: IoEvent, app: &AppArc) {
        match io_event {
            IoEvent::RefreshAuthentication => {
                if let Some(new_token_info) = get_token(self.oauth).await {
                    let (new_spotify, new_token_expiry) = get_spotify(new_token_info);
                    self.spotify = new_spotify;
                    self.spotify_token_expiry = new_token_expiry;
                } else {
                    println!("\nFailed to refresh authentication token");
                    // TODO panic!
                }
            }
            IoEvent::GetPlaylists => {
                let mut app = app.lock().await;
                let playlists = self
                    .spotify
                    .current_user_playlists(app.large_search_limit, None)
                    .await;

                match playlists {
                    Ok(p) => {
                        app.playlists = Some(p);
                        // Select the first playlist
                        app.selected_playlist_index = Some(0);
                    }
                    Err(e) => {
                        app.handle_error(e);
                    }
                };

                self.get_user(&mut app).await;
            }
            IoEvent::GetDevices => {
                self.get_devices(&app).await;
            }
            IoEvent::GetCurrentPlayback => {
                self.get_current_playback(&app).await;
            }
        };
    }

    pub async fn get_user(&self, app: &mut App) {
        match self.spotify.current_user().await {
            Ok(user) => {
                app.user = Some(user);
            }
            Err(e) => {
                app.handle_error(e);
            }
        }
    }

    pub async fn get_devices(&self, app: &AppArc) {
        let mut app = app.lock().await;
        if let Ok(result) = self.spotify.device().await {
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
        let mut app = app.lock().await;
        match self.spotify.current_user_saved_tracks_contains(&ids).await {
            Ok(is_saved_vec) => {
                for (i, id) in ids.iter().enumerate() {
                    if let Some(is_liked) = is_saved_vec.get(i) {
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
                app.handle_error(e);
            }
        }
    }
}
