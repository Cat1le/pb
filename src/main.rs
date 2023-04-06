use std::sync::Arc;

use std::{cmp, env, fs::File, path::PathBuf, time::Duration};

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

use rand::distributions::uniform::{UniformDuration, UniformSampler};
use rand::rngs::StdRng;
use rand::SeedableRng;
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
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
    let sleep = SleepPerformer::new(UniformDuration::new_inclusive(
        Duration::from_secs(65),
        Duration::from_secs(180),
    ));
    let handles = FuturesUnordered::new();
    for (i, url) in config.bots.into_iter().enumerate() {
        let bot = Bot::new(i as i32, url, pixel.clone(), sleep.clone()).await?;
        handles.push(tokio::spawn(async move { bot.run().await }));
    }
    handles.collect::<Vec<_>>().await;
    Ok(())
}

#[derive(Clone)]
struct SleepPerformer {
    rng: Arc<Mutex<StdRng>>,
    uniform: UniformDuration,
}

impl SleepPerformer {
    fn new(uniform: UniformDuration) -> Self {
        Self {
            rng: Arc::new(Mutex::new(StdRng::from_entropy())),
            uniform,
        }
    }

    async fn perform(&mut self) -> JoinHandle<()> {
        let duration = self.uniform.sample(&mut *self.rng.lock().await);
        tokio::spawn(async move {
            tokio::time::sleep(duration).await;
        })
    }
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
    sleep: SleepPerformer,
    connection: WStream,
}

impl Bot {
    async fn new(
        id: i32,
        url: Url,
        pixel: Arc<Mutex<PixelProvider>>,
        sleep: SleepPerformer,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            id,
            url: url.clone(),
            pixel,
            sleep,
            connection: Self::connect(&url).await?,
        })
    }

    async fn connect(url: &Url) -> anyhow::Result<WStream> {
        Ok(connect_async(url).await.map(|x| x.0)?)
    }

    async fn run(mut self) {
        info!("Worker #{} started.", self.id);
        let mut timer = tokio::spawn(async {});
        while let Some(msg) = self.connection.next().await {
            match msg {
                Ok(tungstenite::Message::Close(..))
                | Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    info!(
                        "Worker #{} connection was closed; trying to reconnect.",
                        self.id,
                    );
                    self.connection = Self::connect(&self.url).await.unwrap();
                    info!("Worker #{} successfully reconnected", self.id);
                    continue;
                }
                // Idk how to deal. C'mon, just ignore
                Err(why) => {
                    error!(
                        "Worker #{} received unexpected error: {}; exiting.",
                        self.id, why
                    );
                    break;
                }
                _ => {}
            }
            if timer.is_finished() {
                match self.pixel.lock().await.get_pixel() {
                    Some(pixel) => {
                        info!(
                            "Worker #{} painting {{{}:{}}} to {}",
                            self.id, pixel.x, pixel.y, pixel.color_id
                        );
                        for i in 0..5 {
                            if let Err(why) = self
                                .connection
                                .send(PixelProvider::pack(pixel.clone()).into())
                                .await
                            {
                                error!(
                                    "Worker #{} cannot send data: {why}; attempt {}/5",
                                    self.id,
                                    i + 1
                                );
                                tokio::time::sleep(Duration::from_secs(5)).await;
                            } else {
                                break;
                            }
                        }
                    }
                    None => return,
                }
                timer = self.sleep.perform().await;
            }
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

struct PixelProvider {
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
    fn new(image: PathBuf, x: u32, y: u32) -> anyhow::Result<Self> {
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

    #[allow(clippy::erasing_op)]
    fn pack(info: PixelInfo) -> Vec<u8> {
        let PixelInfo { x, y, color_id } = info;
        let value = x as i32
            + y as i32 * Self::MAX_WIDTH
            + Self::SIZE * (color_id as i32 + 0 * Self::MAX_COLOR_ID);
        value.to_le_bytes().into()
    }

    fn resolve_color_id(r: u8, g: u8, b: u8) -> ColorId {
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

    fn get_pixel(&mut self) -> Option<PixelInfo> {
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

    fn get_packed_pixel(&mut self) -> Option<Vec<u8>> {
        let info = match self.get_pixel() {
            Some(pixel) => pixel,
            None => return None,
        };
        Some(Self::pack(info))
    }
}

#[derive(Debug)]
struct ColorId {
    id: u8,
    exact: bool,
}

#[derive(Clone)]
struct PixelInfo {
    x: u32,
    y: u32,
    color_id: u8,
}
