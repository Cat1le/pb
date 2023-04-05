use std::{cmp, env, fs::File, path::PathBuf, sync::Arc, time::Duration};

use anyhow::anyhow;
use async_tungstenite::tungstenite;
use async_tungstenite::{
    stream::Stream,
    tokio::{connect_async, TokioAdapter},
    WebSocketStream,
};
use futures::{stream::FuturesUnordered, SinkExt, StreamExt};
use image::DynamicImage;
use lazy_static::lazy_static;
use log::*;

use rand::{Rng, SeedableRng};
use serde::Deserialize;
use tokio::sync::Mutex;
use url::Url;

#[derive(Deserialize)]
struct Config {
    brush: Brush,
    bots: Vec<Url>,
}

#[derive(Deserialize)]
struct Brush {
    image: PathBuf,
    offset_x: u32,
    offset_y: u32,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    drop(dotenvy::dotenv());
    pretty_env_logger::init();
    let config_file = File::open(env::var("PB_CONFIG").unwrap_or_else(|_| {
        info!("PB_CONFIG var were not present, using default path (pb.json)");
        "pb.json".into()
    }))?;
    let config = serde_json::from_reader::<_, Config>(config_file)?;
    let pixel = Arc::new(Mutex::new(PixelProvider::new(
        config.brush.image,
        config.brush.offset_x,
        config.brush.offset_y,
    )?));
    let handles = FuturesUnordered::new();
    for (i, url) in config.bots.into_iter().enumerate() {
        let bot = Bot::new(i as i32, url, pixel.clone()).await?;
        handles.push(tokio::spawn(async move { bot.start().await }));
    }
    handles.collect::<Vec<_>>().await;
    Ok(())
}

type WStream = WebSocketStream<
    Stream<
        TokioAdapter<tokio::net::TcpStream>,
        TokioAdapter<tokio_native_tls::TlsStream<tokio::net::TcpStream>>,
    >,
>;

struct Bot {
    id: i32,
    url: Url,
    pixel: Arc<Mutex<PixelProvider>>,
    connection: Mutex<WStream>,
}

impl Bot {
    async fn new(id: i32, url: Url, pixel: Arc<Mutex<PixelProvider>>) -> anyhow::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            id,
            url: url.clone(),
            pixel,
            connection: Mutex::new(Self::connect(&url).await?),
        }))
    }

    async fn connect(url: &Url) -> anyhow::Result<WStream> {
        Ok(connect_async(url).await.map(|x| x.0)?)
    }

    async fn start(self: Arc<Self>) {
        info!("Worker #{} started", self.id);
        let this = self.clone();
        tokio::spawn(async move {
            while let Some(msg) = this.connection.lock().await.next().await {
                if matches!(
                    msg,
                    Ok(tungstenite::Message::Close(..))
                        | Err(tungstenite::Error::ConnectionClosed
                            | tungstenite::Error::AlreadyClosed)
                ) {
                    info!(
                        "Worker #{} had troubles with connection ({}), trying to reconnect.",
                        this.id,
                        msg.unwrap_err()
                    );
                    *this.connection.lock().await = Self::connect(&this.url).await.unwrap();
                    info!("Worker #{} successfully reconnected", this.id);
                }
            }
        });
        while let Some(msg) = self.pixel.lock().await.get_packed_pixel() {
            drop(self.connection.lock().await.send(msg.into()).await);
            let mut rng = rand::rngs::StdRng::from_entropy();
            tokio::time::sleep(Duration::from_secs(rng.gen_range(65..=180))).await;
        }
    }
}

lazy_static! {
    static ref COLORS: Vec<(u8, u8, u8)> = [
        "#FFFFFF", "#C2C2C2", "#858585", "#474747", "#000000", "#3AAFFF", "#71AAEB", "#4A76A8",
        "#074BF3", "#5E30EB", "#FF6C5B", "#FE2500", "#FF218B", "#99244F", "#4D2C9C", "#FFCF4A",
        "#FEB43F", "#FE8648", "#FF5B36", "#DA5100", "#94E044", "#5CBF0D", "#C3D117", "#FCC700",
        "#D38301",
    ]
    .into_iter()
    .map(|x| (
        u8::from_str_radix(&x[1..3], 16).unwrap(),
        u8::from_str_radix(&x[3..5], 16).unwrap(),
        u8::from_str_radix(&x[5..], 16).unwrap()
    ))
    .collect::<Vec<_>>();
}

pub struct PixelProvider {
    image: DynamicImage,
    initial: (u32, u32),
    current: (u32, u32),
    end: bool,
}

impl PixelProvider {
    const MAX_COLOR_ID: i32 = 25;
    const MAX_HEIGHT: i32 = 400;
    const MAX_WIDTH: i32 = 1590;
    const SIZE: i32 = 636000;

    #[allow(clippy::new_ret_no_self)]
    pub fn new(image: PathBuf, x: u32, y: u32) -> anyhow::Result<Self> {
        if x >= Self::MAX_WIDTH as u32 {
            Err(anyhow!("X axis is out of range"))?
        }
        if y >= Self::MAX_HEIGHT as u32 {
            Err(anyhow!("Y axis is out of range"))?
        }
        Ok(Self {
            image: ::image::open(image)?,
            initial: (x, y),
            current: (x, y),
            end: false,
        })
    }

    pub fn resolve_color_id(r: u8, g: u8, b: u8) -> ColorId {
        let mut nearest = None;
        for (index, (r1, g1, b1)) in COLORS.iter().enumerate() {
            let temp = ((cmp::max(r, *r1) - cmp::min(r, *r1)) as u32).pow(2)
                + ((cmp::max(g, *g1) - cmp::min(g, *g1)) as u32).pow(2)
                + ((cmp::max(b, *b1) - cmp::min(b, *b1)) as u32).pow(2);
            if temp == 0 {
                return ColorId {
                    id: index as u8,
                    exact: true,
                };
            }
            nearest = nearest.map_or(Some((index, temp)), |(c, t)| {
                if temp < t {
                    Some((index, temp))
                } else {
                    Some((c, t))
                }
            });
        }
        ColorId {
            id: nearest.unwrap().0 as u8,
            exact: false,
        }
    }

    pub fn get_pixel(&mut self) -> Option<PixelInfo> {
        if self.end {
            return None;
        }
        let (dx, dy) = (
            self.current.0 - self.initial.0,
            self.current.1 - self.initial.1,
        );
        let rgb = self
            .image
            .as_rgb8()
            .expect("Cannot represent given image as rgb8");
        let (width, height) = rgb.dimensions();
        if dx >= width {
            self.current.0 = 0;
            self.current.1 += 1;
        }
        if dy >= height {
            self.end = true;
            return None;
        }
        let [r, g, b] = rgb.get_pixel(dx, dy).0;
        let ColorId { id, exact } = Self::resolve_color_id(r, g, b);
        if !exact {
            warn!("Pixel {{{dx}:{dy}}} is not exactly match allowed colors. Converted to {id:x}");
        }
        Some(PixelInfo {
            x: self.current.0,
            y: self.current.1,
            color_id: id,
        })
    }

    #[allow(clippy::erasing_op)]
    pub fn get_packed_pixel(&mut self) -> Option<Vec<u8>> {
        let PixelInfo { x, y, color_id } = match self.get_pixel() {
            Some(pixel) => pixel,
            None => return None,
        };
        let value = x as i32
            + y as i32 * Self::MAX_WIDTH
            + Self::SIZE * (color_id as i32 + 0 * Self::MAX_COLOR_ID);
        Some(value.to_le_bytes().into())
    }
}

#[derive(Debug)]
pub struct ColorId {
    pub id: u8,
    pub exact: bool,
}

pub struct PixelInfo {
    pub x: u32,
    pub y: u32,
    pub color_id: u8,
}
