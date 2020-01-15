use std::path::{Path, PathBuf};
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Write};
use std::sync::{Arc};
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::{HashMap};
use std::net::SocketAddr;
use std::process::{Command, Child, Stdio};
use std::thread::{self};
use std::sync::mpsc;

use actix::{Actor, Running, Message, Handler, StreamHandler, ActorContext, AsyncContext};
use actix_web::{web, http, App, HttpRequest, HttpResponse, HttpServer, Responder, body::Body};
use actix_web_actors::ws;
use actix_files;
use bytes::Bytes;
use serde_derive::{Deserialize, Serialize};
use clap;
use toml;

#[derive(Serialize, Deserialize)]
struct GamesResponse {
    games: Vec<GameResponse>,
}

#[derive(Clone, Serialize, Deserialize)]
struct GameResponse {
    id: String,
    name: String,
    author: String,
    description: String,
    play: GamePlayDetailsResponse,
}

#[derive(Clone, Serialize, Deserialize)]
struct GamePlayDetailsResponse {
    url: String,
}

struct ServerState {
    games_response_json: Vec<u8>,
    games_dir: PathBuf,
    configs_by_id: HashMap<String, GameConfig>,
}

#[derive(Clone, Deserialize)]
struct GameConfig {
    id: String,
    name: String,
    description: String,
    author: String,
    application: String,
    args: Vec<String>,
}

fn to_game_response(config: &GameConfig) -> GameResponse {
    GameResponse {
        id: config.id.clone(),
        name: config.name.clone(),
        author: config.author.clone(),
        description: config.description.clone(),
        play: GamePlayDetailsResponse {
            url: format!("/play/{}", config.id),
        }
    }
}

fn to_games_json(configs: &Vec<GameConfig>) -> Vec<u8> {
    let resp = GamesResponse {
        games: configs.iter().map(|cfg| to_game_response(cfg)).collect(),
    };

    serde_json::to_vec_pretty(&resp).unwrap()
}

fn should_ignore(game_dirname: &str) -> bool {
    if let Some(c) = game_dirname.chars().next() {
        match c {
            '.' => true,
            '#' => true,
            _ => false,
        }
    } else {
        false
    }
}

fn load_config(config_path: &Path) -> std::io::Result<GameConfig> {
    let mut fd = File::open(config_path)?;
    let mut s = String::new();
    fd.read_to_string(&mut s)?;

    Ok(toml::from_str(&s)?)
}

fn load_game_configs(games_dir: &Path) -> std::io::Result<Vec<GameConfig>> {
    if !games_dir.exists() {
        let msg = format!("{}: no such file or directory", games_dir.display());
        return Err(io::Error::new(ErrorKind::NotFound, msg));
    }

    if !games_dir.is_dir() {
        let msg = format!("{}: not a directory", games_dir.display());
        return Err(io::Error::new(ErrorKind::InvalidInput, msg));
    }

    let mut configs = Vec::<GameConfig>::new();

    for entry in fs::read_dir(games_dir)? {
        let entry = entry?;
        let path = entry.path();

        if should_ignore(&path.file_name().unwrap().to_str().unwrap()) {
            continue;
        }

        if !entry.metadata()?.is_dir() {
            continue;
        }

        let cfg_path = path.join("textadventure.toml");

        if cfg_path.is_file() {
            configs.push(load_config(&cfg_path)?);
        }
    }

    Ok(configs)
}

async fn games(st: web::Data<ServerState>) -> impl Responder {
    HttpResponse::Ok()
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(Body::from_slice(&st.games_response_json))
}

enum ProcessSt {
    Booting,
    Booted(BootedProcess),
    Exited,
}

struct BootedProcess {
    process: Child,
    stdin: mpsc::Sender<Vec<u8>>,
}

struct SubprocessActor {
    config: GameConfig,
    game_dir: PathBuf,
    process_st: ProcessSt,
}

struct StdoutMsg {
    bytes: Bytes,
}

impl Message for StdoutMsg {
    type Result = ();
}

struct ExitMsg();

impl Message for ExitMsg {
    type Result = ();
}

impl SubprocessActor {
    fn new(game_dir: PathBuf, config: GameConfig) -> Self {
        Self {
            game_dir: game_dir,
            config: config,
            process_st: ProcessSt::Booting
        }
    }
}

impl Actor for SubprocessActor {
    type Context = ws::WebsocketContext<Self>;

    fn stopping(&mut self, _ctx: &mut Self::Context) -> Running {
        if let ProcessSt::Booted(p) = &mut self.process_st {
            p.process.kill().unwrap();
            p.process.wait().unwrap();
        }
        self.process_st = ProcessSt::Exited;
        Running::Stop
    }
}

impl StreamHandler<Result<ws::Message, ws::ProtocolError>> for SubprocessActor {

    fn started(&mut self, ctx: &mut Self::Context) {
        
        let mut b = Command::new(&self.config.application);
        b.args(self.config.args.iter());
        b.stdin(Stdio::piped());
        b.stdout(Stdio::piped());
        b.stderr(Stdio::piped());
        b.current_dir(&self.game_dir);

        match b.spawn() {
            Ok(mut proc) => {
                let should_send_exit = Arc::new(AtomicBool::new(true));

                let stdout_addr = ctx.address();
                let stderr_addr = stdout_addr.clone();
                let mut stdout = proc.stdout.take().unwrap();
                let mut stderr = proc.stderr.take().unwrap();
                let mut stdin = proc.stdin.take().unwrap();
                let stdout_should_send_exit = should_send_exit.clone();
                let stderr_should_send_exit = should_send_exit;

                thread::Builder::new().name("stdout_thread".to_string()).spawn(move || {
                    let addr = stdout_addr;

                    loop {
                        let mut buf: [u8; 512] = [0; 512];
                        let sz = stdout.read(&mut buf).unwrap_or(0);

                        if sz > 0 {
                            let bytes = Bytes::copy_from_slice(&buf[0..sz]);
                            addr.do_send(StdoutMsg { bytes: bytes });
                        } else {
                            if stdout_should_send_exit.compare_and_swap(true, false, Ordering::Relaxed) {
                                addr.do_send(ExitMsg{});
                            }
                            break;
                        }
                    }
                }).unwrap();

                thread::Builder::new().name("stderr_thread".to_string()).spawn(move || {
                    let addr = stderr_addr;

                    loop {
                        let mut buf: [u8; 512] = [0; 512];
                        let sz = stderr.read(&mut buf).unwrap_or(0);

                        if sz > 0 {
                            let bytes = Bytes::copy_from_slice(&buf[0..sz]);
                            addr.do_send(StdoutMsg { bytes: bytes });
                        } else {
                            if stderr_should_send_exit.compare_and_swap(true, false, Ordering::Relaxed) {
                                addr.do_send(ExitMsg {});
                            }
                            break;
                        }
                    }
                }).unwrap();

                let (tx, rx) = mpsc::channel::<Vec<u8>>();
                thread::Builder::new().name("stdin_thread".to_string()).spawn(move || {
                    loop {
                        match rx.recv() {
                            Ok(buf) => {
                                match stdin.write(&buf) {
                                    Ok(_) => stdin.flush().unwrap(),
                                    Err(_) => break,  // TODO
                                }
                            },
                            Err(_) => break,  // TODO: error propagation
                        }
                    }
                }).unwrap();

                self.process_st = ProcessSt::Booted(BootedProcess {
                    process: proc,
                    stdin: tx,
                });
            },
            Err(e) => {
                ctx.text(e.to_string());
                ctx.stop();
            },
        }
    }

    fn handle(
        &mut self,
        msg: Result<ws::Message, ws::ProtocolError>,
        ctx: &mut Self::Context,
    ) {
        match msg {
            Ok(ws::Message::Ping(msg)) => ctx.pong(&msg),
            Ok(ws::Message::Text(text)) => {
                if let ProcessSt::Booted(p) = &mut self.process_st {
                    p.stdin.send(text.into_bytes()).unwrap();
                }
            },
            Ok(ws::Message::Binary(bin)) => {
                if let ProcessSt::Booted(p) = &mut self.process_st {
                    p.stdin.send(bin.to_vec()).unwrap();
                }
            },
            Ok(ws::Message::Close(_)) => {
                // Firefox sends this *before* closing the TCP
                // channel. Can cause leak-like behavior in Firefox
                // because it'll keep the TCP channel open even after
                // closing tabs etc.
                ctx.close(Option::None);
                ctx.stop();
            },
            _ => (),
        }
    }

    fn finished(&mut self, ctx: &mut Self::Context) {
        ctx.stop();
    }
}

impl Handler<StdoutMsg> for SubprocessActor {
    type Result = ();

    fn handle(&mut self, msg: StdoutMsg, ctx: &mut Self::Context) -> Self::Result {
        ctx.binary(msg.bytes);
    }
}

impl Handler<ExitMsg> for SubprocessActor {
    type Result = ();

    fn handle(&mut self, _msg: ExitMsg, ctx: &mut Self::Context) -> Self::Result {
        ctx.stop();
    }
}

async fn play(st: web::Data<ServerState>,
              req: HttpRequest,
              stream: web::Payload) -> Result<HttpResponse, actix_web::Error> {

    let gameid = req.match_info().query("gameid");
    match st.configs_by_id.get(gameid) {
        Some(cfg) => {
            let game_dir = st.games_dir.join(gameid).join("game-files");
            let actor = SubprocessActor::new(game_dir, cfg.clone());
            ws::start(actor, &req, stream)
        }
        None => Ok(HttpResponse::NotFound().finish()),
    }
}

struct CliArgs {
    games_dir: String,
    port: u16,
}

#[actix_rt::main]
async fn main() -> std::io::Result<()> {
    let cli_args: io::Result<CliArgs> = {
        let matches = clap::App::new("Text")
            .author("Adam Kewley <adamk117@gmail.com>")
            .about("Boots Textadventurer webserver daemon")
            .arg(clap::Arg::with_name("GAMES_DIR")
                 .help("Directory containing game definitions")
                 .required(true))
            .arg(clap::Arg::with_name("port")
                 .short("p")
                 .long("port")
                 .value_name("PORT_NUMBER")
                 .help("TCP port the daemon should listen on")
                 .default_value("8080")
                 .takes_value(true))
            .get_matches();

        let games_dir: String = matches.value_of("GAMES_DIR").unwrap().to_string();

        let port: u16 = {
            let port_str = matches.value_of("port").unwrap();
            let maybe_port_num = match port_str.parse::<u16>() {
                Ok(port) => Ok(port),
                Err(_) => Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{}: not a valid number", port_str))),
            };
            maybe_port_num?
        };

        Ok(CliArgs{ games_dir: games_dir, port: port })
    };
    let cli_args = cli_args?;

    let games_dir = PathBuf::from(&cli_args.games_dir);

    let games_config =
        Arc::new(load_game_configs(&games_dir).unwrap());

    HttpServer::new(move || {
        let st = ServerState {
            games_response_json: to_games_json(&games_config),
            games_dir: games_dir.clone(),
            configs_by_id: games_config.iter().map(|cfg| (cfg.id.clone(), cfg.clone())).collect(),
        };

        App::new()
            .data(st)
            .route("/api/games", web::get().to(games))
            .route("/api/play/{gameid}", web::get().to(play))
            .service(actix_files::Files::new("/", "static/").index_file("index.html"))
    })
    .workers(1)
    .bind(SocketAddr::from(([127, 0, 0, 1], cli_args.port)))?
    .run()
    .await
}
