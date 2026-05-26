use warp::Filter;
use serde::{Deserialize, Serialize};
use reqwest::Client;
use warp::filters::BoxedFilter;



#[allow(dead_code)]
#[derive(Deserialize)]
struct TokenSwapRequest {
    code: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct SpotifyTokenResponse {
    access_token: String,
    token_type: String,
    expires_in: i32,
    refresh_token: Option<String>,
}

#[allow(dead_code)]
#[derive(Deserialize)]
struct TokenRefreshRequest {
    refresh_token: String,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
struct SpotifyErrorResponse {
    error: String,
    error_description: String,
}

#[allow(dead_code)]
#[derive(Debug)]
enum CustomError {
    InvalidResponse,
    RequestFailed,
}

impl warp::reject::Reject for CustomError {}

#[allow(dead_code)]
const CLIENT_ID: &'static str = "7807e251b8cb4f6da94f76e06fe336f2";
#[allow(dead_code)]
const CLIENT_SECRET: &'static str = "139ad3c07606463cba431eeae6f8c35e";
#[allow(dead_code)]
const REDIRECT_URI: &'static str = "audioshare://spotifyAuth";

#[allow(dead_code)]
pub fn get_routes() -> BoxedFilter<(impl warp::Reply,)>{
    let token_swap = warp::post()
        .and(warp::path("spotifyTokenSwap"))
        .and(warp::body::form())
        .and_then(move |req: TokenSwapRequest| {
        let client_id = CLIENT_ID.to_string();
        let client_secret = CLIENT_SECRET.to_string();
        let redirect_uri = REDIRECT_URI.to_string();
        println!("{}", client_id);
        async move {
            let client = Client::new();
            let params = [
                ("grant_type", "authorization_code"),
                ("code", &req.code),
                ("redirect_uri", &redirect_uri),
                ("client_id", &client_id),
                ("client_secret", &client_secret),
            ];

            let res = client
                .post("https://accounts.spotify.com/api/token")
                .form(&params)
                .send()
                .await;

            match res {
                Ok(response) => {
                    let text = response.text().await.unwrap_or_else(|_| "Failed to read response body".to_string());
                    println!("Raw response body: {}", text);

                    // Try to parse the response as the expected JSON structure
                    let spotify_response: Result<SpotifyTokenResponse, _> = serde_json::from_str(&text);
                    match spotify_response {
                        Ok(tokens) => Ok(warp::reply::json(&tokens)),
                        Err(_) => {
                            // Try to parse the response as an error message
                            let error_response: Result<SpotifyErrorResponse, _> = serde_json::from_str(&text);
                            match error_response {
                                Ok(error) => {
                                    println!("Spotify error response: {:?}", error);
                                    Err(warp::reject::custom(CustomError::InvalidResponse))
                                }
                                Err(_) => {
                                    println!("Failed to parse response: {}", text);
                                    Err(warp::reject::custom(CustomError::InvalidResponse))
                                }
                            }
                        }
                    }
                }
                Err(_) => {
                    println!("Error");
                    return Err(warp::reject::custom(CustomError::RequestFailed));
                }
            }
        }
        });

    let token_refresh = warp::post()
        .and(warp::path("spotifyTokenRefresh"))
        .and(warp::body::json())
        .and_then(move |req: TokenRefreshRequest| {
        let client_id = CLIENT_ID.to_string();
        let client_secret = CLIENT_SECRET.to_string();
        println!("TOKEN REFRESH");
        async move {
            let client = Client::new();
            let params = [
                ("grant_type", "refresh_token"),
                ("refresh_token", &req.refresh_token),
                ("client_id", &client_id),
                ("client_secret", &client_secret),
            ];

            let res = client
                .post("https://accounts.spotify.com/api/token")
                .form(&params)
                .send()
                .await;

            match res {
                Ok(response) => {
                    let spotify_response: Result<SpotifyTokenResponse, _> =
                        response.json().await;
                    match spotify_response {
                        Ok(tokens) => Ok(warp::reply::json(&tokens)),
                        Err(_) => Err(warp::reject::custom(CustomError::InvalidResponse)),
                    }
                }
                Err(_) => Err(warp::reject::custom(CustomError::RequestFailed)),
            }
        }
        });

    token_swap.or(token_refresh).boxed()
}



