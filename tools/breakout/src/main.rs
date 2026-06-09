//! Breakout demo over VGE.
//!
//! Run inside veter. Controls: A / Left = paddle left, D / Right =
//! paddle right, Space = restart after game-over, Q or Ctrl-C = quit.
//!
//! The game wraps itself in the alternate screen (DECSET 1049) so the
//! user's shell history is restored on exit. Each frame's render-state
//! delta is shipped as a single VGE envelope; veter's response is
//! drained inline to keep the PTY buffer from filling.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use vge_protocol::apc::ApcStream;
use vge_protocol::codec::{Point, Reader, Rect, Transform};
use vge_protocol::command::{
    Align, Color, Command, CreateElementBody, DrawCmd, FontStyle, Style, UpdateCommandBody,
    UpdateTextBody, UpdateTextRange,
};
use vge_protocol::encode::build_envelope;
use vge_protocol::frame::*;
use vge_protocol::path::{PathNode, PathSegment};

const FRAME_DT: Duration = Duration::from_millis(33); // ~30 Hz
const TIMEOUT: Duration = Duration::from_millis(500);

const FIELD_W: f32 = 60.0;
const FIELD_H: f32 = 25.0;

const PADDLE_W: f32 = 8.0;
const PADDLE_H: f32 = 0.8;
const PADDLE_Y: f32 = FIELD_H - 1.5;
const PADDLE_STEP: f32 = 4.0;

const BALL_R: f32 = 0.45;
const BALL_INITIAL_SPEED: f32 = 0.3;
const BALL_SPEED_PER_BRICK: f32 = 0.006;
const BALL_SPEED_PER_TICK: f32 = 0.0004;
const BALL_MAX_SPEED: f32 = 1.0;

const BRICK_ROWS: usize = 5;
const BRICK_COLS: usize = 12;
const BRICK_W: f32 = FIELD_W / BRICK_COLS as f32;
const BRICK_H: f32 = 1.2;
const BRICK_PAD: f32 = 0.12;
const BRICK_RADIUS: f32 = 0.25;
const BRICK_TOP: f32 = 2.0;

const SPARK_LIFE: u32 = 18;
const SPARKS_PER_BRICK: usize = 10;
/// Max per-frame spark rotation, radians (§9.11 transform tumble).
const SPARK_MAX_SPIN: f32 = 0.45;

/// Paddle squash-on-impact animation (§9.11): frames it lasts and the
/// peak scale deviation (x grows by k while y shrinks by k).
const PADDLE_SQUASH_FRAMES: u8 = 6;
const PADDLE_SQUASH_K: f32 = 0.28;

fn main() -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("breakout must run with stdin and stdout connected to a terminal");
    }

    let mut tty = TtyGuard::enable()?;
    drain_stale_stdin();

    // Probe before entering alt screen — cell pixel dims don't change
    // across screen switches and the probe response gives us the
    // anisotropic cell ratio we need to draw a visually-circular ball.
    let probe = run_probe(TIMEOUT)?
        .ok_or_else(|| anyhow!("VGE probe timed out — terminal does not appear to support VGE"))?;

    tty.enter_alt_screen()?;

    let mut game = Game::new(probe.cell_pixel_width as f32, probe.cell_pixel_height as f32);
    send_and_drain(&game.initial_commands(), TIMEOUT)?;

    let mut apc = ApcStream::with_marker(*MARKER_T2C);
    let mut last_frame = Instant::now();
    let mut buf = [0u8; 4096];

    loop {
        let now = Instant::now();
        if now.duration_since(last_frame) >= FRAME_DT {
            last_frame = now;
            let cmds = game.tick();
            if !cmds.is_empty() {
                send_and_drain(&cmds, TIMEOUT)?;
            }
            if game.quit {
                break;
            }
        }

        let until_next = FRAME_DT.saturating_sub(Instant::now() - last_frame);
        if poll_stdin_for(until_next)? {
            let n = read_stdin(&mut buf)?;
            if n == 0 {
                continue;
            }
            let out = apc.feed(&buf[..n]);
            // out.payloads = response envelopes (already drained
            // inside send_and_drain ideally, but mop up stragglers).
            let _ = out.payloads;
            game.handle_input(&out.passthrough);
        }
    }

    Ok(())
}

// --- Game ---

struct Game {
    paddle_x: f32,
    ball_x: f32,
    ball_y: f32,
    ball_vx: f32,
    ball_vy: f32,
    speed: f32,
    bricks: Vec<Brick>,
    sparks: Vec<Spark>,
    next_spark_id: u64,
    score: u32,
    score_dirty: bool,
    state: GameState,
    quit: bool,
    paddle_dirty: bool,
    /// Frames left in the squash-on-impact animation; 0 = at rest.
    paddle_squash: u8,
    msg_visible: bool,
    msg_text: String,
    /// Cell pixel width / height used to compensate the ball's
    /// elliptic radii so it draws as a visual circle.
    cell_pw: f32,
    cell_ph: f32,
    /// Brick storage keys whose elements should be deleted in the
    /// next tick (collected during physics, drained when emitting
    /// the frame's command diff).
    pending_brick_deletes: Vec<String>,
    /// Set by `restart()`. The next `tick()` emits `ClearAll` +
    /// `initial_commands()` to bring the engine in sync with the
    /// freshly-rebuilt game state, then clears the flag.
    needs_full_resync: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GameState {
    Playing,
    Over,
    Won,
}

struct Brick {
    id: String,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    color: Color,
    alive: bool,
}

struct Spark {
    id: String,
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    color: Color,
    life: u32,
    just_spawned: bool,
    /// Current rotation (radians) and per-frame spin, rendered via
    /// UpdateTransform — the spark square tumbles as it flies.
    angle: f32,
    spin: f32,
}

impl Game {
    fn new(cell_pw: f32, cell_ph: f32) -> Self {
        let mut g = Self {
            paddle_x: FIELD_W * 0.5 - PADDLE_W * 0.5,
            ball_x: FIELD_W * 0.5,
            ball_y: PADDLE_Y - 1.5,
            ball_vx: BALL_INITIAL_SPEED * 0.7,
            ball_vy: -BALL_INITIAL_SPEED,
            speed: BALL_INITIAL_SPEED,
            bricks: Vec::new(),
            sparks: Vec::new(),
            next_spark_id: 0,
            score: 0,
            score_dirty: true,
            state: GameState::Playing,
            quit: false,
            paddle_dirty: true,
            paddle_squash: 0,
            msg_visible: false,
            msg_text: String::new(),
            cell_pw: cell_pw.max(1.0),
            cell_ph: cell_ph.max(1.0),
            pending_brick_deletes: Vec::new(),
            needs_full_resync: false,
        };
        g.build_bricks();
        g
    }

    fn build_bricks(&mut self) {
        let palette = [
            color(0xE6_3946_FF),
            color(0xF1_FA_3CFF),
            color(0x06_D6_A0_FF),
            color(0x11_8A_B2_FF),
            color(0xA8_9E_FE_FF),
        ];
        for row in 0..BRICK_ROWS {
            for col in 0..BRICK_COLS {
                let x = col as f32 * BRICK_W;
                let y = BRICK_TOP + row as f32 * BRICK_H;
                self.bricks.push(Brick {
                    id: format!("br-{row}-{col}"),
                    x,
                    y,
                    w: BRICK_W,
                    h: BRICK_H,
                    color: palette[row.min(palette.len() - 1)],
                    alive: true,
                });
            }
        }
    }

    fn initial_commands(&self) -> Vec<(Command, u32)> {
        let mut cmds = Vec::new();

        // Background field (dim panel).
        cmds.push((
            Command::CreateElement(CreateElementBody {
                id: "field".into(),
                commands: vec![DrawCmd::FillRectangles {
                    fill: Style::Flat(color(0x12_18_26_FF)),
                    rects: vec![Rect {
                        x: 0.0,
                        y: 0.0,
                        w: FIELD_W,
                        h: FIELD_H,
                    }],
                }],
                origin: Point { x: 0.0, y: 0.0 },
                is_visible: true,
                draw_order: -10,
                parent: None,
                size: None,
                transform: None,
            }),
            0,
        ));

        // Bricks — one element each, so a destroyed brick is just a
        // single DeleteElement and a small spark burst.
        for b in &self.bricks {
            cmds.push((
                Command::CreateElement(CreateElementBody {
                    id: b.id.clone(),
                    commands: vec![brick_drawcmd(b, self.cell_pw, self.cell_ph)],
                    origin: Point { x: b.x, y: b.y },
                    is_visible: true,
                    draw_order: 0,
                    parent: None,
                    size: None,
                    transform: None,
                }),
                0,
            ));
        }

        // Paddle.
        cmds.push((
            Command::CreateElement(CreateElementBody {
                id: "paddle".into(),
                commands: vec![paddle_drawcmd()],
                origin: Point {
                    x: self.paddle_x,
                    y: PADDLE_Y,
                },
                is_visible: true,
                draw_order: 5,
                parent: None,
                size: None,
                transform: None,
            }),
            0,
        ));

        // Ball.
        cmds.push((
            Command::CreateElement(CreateElementBody {
                id: "ball".into(),
                commands: vec![ball_drawcmd(self.cell_pw, self.cell_ph)],
                origin: Point {
                    x: self.ball_x,
                    y: self.ball_y,
                },
                is_visible: true,
                draw_order: 10,
                parent: None,
                size: None,
                transform: None,
            }),
            0,
        ));

        // Score text — right-aligned at top-right of the field.
        cmds.push((
            Command::CreateElement(CreateElementBody {
                id: "score".into(),
                commands: vec![DrawCmd::DrawText {
                    origin: Point {
                        x: FIELD_W - 0.5,
                        y: 0.6,
                    },
                    align: Align::Right,
                    fill: Style::Flat(color(0xFF_FF_FF_FF)),
                    font_style: FontStyle(0x00),
                    text: format!("Score: {}", self.score),
                }],
                origin: Point { x: 0.0, y: 0.0 },
                is_visible: true,
                draw_order: 20,
                parent: None,
                size: None,
                transform: None,
            }),
            0,
        ));

        // Centered message — invisible until game over / win.
        cmds.push((
            Command::CreateElement(CreateElementBody {
                id: "msg".into(),
                commands: vec![DrawCmd::DrawText {
                    origin: Point {
                        x: FIELD_W * 0.5,
                        y: FIELD_H * 0.5,
                    },
                    align: Align::Center,
                    fill: Style::Flat(color(0xFF_FF_FF_FF)),
                    font_style: FontStyle(0x00),
                    text: "GAME OVER".into(),
                }],
                origin: Point { x: 0.0, y: 0.0 },
                is_visible: false,
                draw_order: 30,
                parent: None,
                size: None,
                transform: None,
            }),
            0,
        ));

        cmds
    }

    fn handle_input(&mut self, bytes: &[u8]) {
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            match b {
                b'q' | b'Q' | 0x03 => self.quit = true,
                b'a' | b'h' => self.move_paddle(-PADDLE_STEP),
                b'd' | b'l' => self.move_paddle(PADDLE_STEP),
                b' ' if self.state != GameState::Playing => self.restart(),
                0x1B if i + 2 < bytes.len() && bytes[i + 1] == b'[' => {
                    match bytes[i + 2] {
                        b'C' => self.move_paddle(PADDLE_STEP),
                        b'D' => self.move_paddle(-PADDLE_STEP),
                        _ => {}
                    }
                    i += 2;
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn move_paddle(&mut self, dx: f32) {
        if self.state != GameState::Playing {
            return;
        }
        let new_x = (self.paddle_x + dx).clamp(0.0, FIELD_W - PADDLE_W);
        if (new_x - self.paddle_x).abs() > 1e-4 {
            self.paddle_x = new_x;
            self.paddle_dirty = true;
        }
    }

    fn restart(&mut self) {
        let pw = self.cell_pw;
        let ph = self.cell_ph;
        *self = Game::new(pw, ph);
        self.needs_full_resync = true;
    }

    /// Run one frame of physics + collisions and return the VGE
    /// command diff for the renderer.
    fn tick(&mut self) -> Vec<(Command, u32)> {
        // After a restart, every element from the previous run still
        // lives in the engine's element table. Wipe it and re-emit the
        // initial scene before doing anything else this frame.
        if self.needs_full_resync {
            self.needs_full_resync = false;
            self.score_dirty = false;
            self.paddle_dirty = false;
            self.pending_brick_deletes.clear();
            let mut cmds: Vec<(Command, u32)> = Vec::new();
            cmds.push((Command::ClearAll, 0));
            cmds.extend(self.initial_commands());
            return cmds;
        }

        let mut cmds = Vec::new();

        if matches!(self.state, GameState::Playing) {
            self.step_physics();
        }

        // Drain brick destructions surfaced during physics.
        for id in self.pending_brick_deletes.drain(..) {
            cmds.push((Command::DeleteElement { id }, 0));
        }

        // Spark update / cleanup. Newly-spawned sparks emit
        // CreateElement; ongoing ones get UpdateOrigin; expired ones
        // emit DeleteElement and are removed from the list.
        let mut alive_sparks = Vec::with_capacity(self.sparks.len());
        for mut s in self.sparks.drain(..) {
            if matches!(self.state, GameState::Playing) {
                s.x += s.vx;
                s.y += s.vy;
                s.vx *= 0.92;
                s.vy = s.vy * 0.92 + 0.04; // tiny gravity
                s.angle += s.spin;
                if s.life > 0 {
                    s.life -= 1;
                }
            }
            // Spark squares are origin-centered, so the tumble is a pure
            // rotation about the element origin (§9.13).
            let tumble = Transform::rotate_about(s.angle, 0.0, 0.0, self.cell_pw, self.cell_ph);
            if s.just_spawned {
                cmds.push((
                    Command::CreateElement(CreateElementBody {
                        id: s.id.clone(),
                        commands: vec![spark_drawcmd(s.color)],
                        origin: Point { x: s.x, y: s.y },
                        is_visible: true,
                        draw_order: 15,
                        parent: None,
                        size: None,
                        transform: Some(tumble),
                    }),
                    0,
                ));
                s.just_spawned = false;
                alive_sparks.push(s);
            } else if s.life == 0 {
                cmds.push((Command::DeleteElement { id: s.id.clone() }, 0));
            } else {
                cmds.push((
                    Command::UpdateOrigin {
                        id: s.id.clone(),
                        origin: Point { x: s.x, y: s.y },
                    },
                    0,
                ));
                cmds.push((
                    Command::UpdateTransform {
                        id: s.id.clone(),
                        transform: tumble,
                    },
                    0,
                ));
                alive_sparks.push(s);
            }
        }
        self.sparks = alive_sparks;

        // Ball position — every frame while playing.
        if matches!(self.state, GameState::Playing) {
            cmds.push((
                Command::UpdateOrigin {
                    id: "ball".into(),
                    origin: Point {
                        x: self.ball_x,
                        y: self.ball_y,
                    },
                },
                0,
            ));
        }

        if self.paddle_dirty {
            cmds.push((
                Command::UpdateOrigin {
                    id: "paddle".into(),
                    origin: Point {
                        x: self.paddle_x,
                        y: PADDLE_Y,
                    },
                },
                0,
            ));
            self.paddle_dirty = false;
        }

        // Squash-on-impact: scale about the paddle's center, easing back
        // to identity over PADDLE_SQUASH_FRAMES (§9.11).
        if self.paddle_squash > 0 && matches!(self.state, GameState::Playing) {
            self.paddle_squash -= 1;
            let k = PADDLE_SQUASH_K * self.paddle_squash as f32 / PADDLE_SQUASH_FRAMES as f32;
            let t = if self.paddle_squash == 0 {
                Transform::IDENTITY
            } else {
                Transform::scale_about(1.0 + k, 1.0 - k, PADDLE_W * 0.5, PADDLE_H * 0.5)
            };
            cmds.push((
                Command::UpdateTransform {
                    id: "paddle".into(),
                    transform: t,
                },
                0,
            ));
        }

        if self.score_dirty {
            cmds.push((
                Command::UpdateText(UpdateTextBody {
                    id: "score".into(),
                    command_index: 0,
                    range: UpdateTextRange::Whole,
                    replacement: format!("Score: {}", self.score),
                }),
                0,
            ));
            self.score_dirty = false;
        }

        // Surface end-state once.
        if !matches!(self.state, GameState::Playing) && !self.msg_visible {
            self.msg_text = match self.state {
                GameState::Won => "YOU WIN  —  press SPACE".into(),
                GameState::Over => "GAME OVER  —  press SPACE".into(),
                GameState::Playing => unreachable!(),
            };
            cmds.push((
                Command::UpdateCommand(UpdateCommandBody {
                    id: "msg".into(),
                    index: 0,
                    command: DrawCmd::DrawText {
                        origin: Point {
                            x: FIELD_W * 0.5,
                            y: FIELD_H * 0.5,
                        },
                        align: Align::Center,
                        fill: Style::Flat(color(0xFF_FF_FF_FF)),
                        font_style: FontStyle(0x00),
                        text: self.msg_text.clone(),
                    },
                }),
                0,
            ));
            cmds.push((
                Command::UpdateVisibility {
                    id: "msg".into(),
                    is_visible: true,
                },
                0,
            ));
            self.msg_visible = true;
        } else if matches!(self.state, GameState::Playing) && self.msg_visible {
            // After a restart.
            cmds.push((
                Command::UpdateVisibility {
                    id: "msg".into(),
                    is_visible: false,
                },
                0,
            ));
            self.msg_visible = false;
        }

        cmds
    }

    fn step_physics(&mut self) {
        // Bump speed each tick; renormalise velocity after.
        self.speed = (self.speed + BALL_SPEED_PER_TICK).min(BALL_MAX_SPEED);
        let v = (self.ball_vx * self.ball_vx + self.ball_vy * self.ball_vy).sqrt();
        if v > 1e-4 {
            let s = self.speed / v;
            self.ball_vx *= s;
            self.ball_vy *= s;
        }

        // Move.
        self.ball_x += self.ball_vx;
        self.ball_y += self.ball_vy;

        // Walls.
        if self.ball_x - BALL_R < 0.0 {
            self.ball_x = BALL_R;
            self.ball_vx = self.ball_vx.abs();
        } else if self.ball_x + BALL_R > FIELD_W {
            self.ball_x = FIELD_W - BALL_R;
            self.ball_vx = -self.ball_vx.abs();
        }
        if self.ball_y - BALL_R < 0.0 {
            self.ball_y = BALL_R;
            self.ball_vy = self.ball_vy.abs();
        }

        // Paddle.
        let p_top = PADDLE_Y;
        let p_bot = PADDLE_Y + PADDLE_H;
        let p_left = self.paddle_x;
        let p_right = self.paddle_x + PADDLE_W;
        if self.ball_y + BALL_R >= p_top
            && self.ball_y - BALL_R <= p_bot
            && self.ball_x + BALL_R >= p_left
            && self.ball_x - BALL_R <= p_right
            && self.ball_vy > 0.0
        {
            self.ball_y = p_top - BALL_R - 0.001;
            self.ball_vy = -self.ball_vy.abs();
            // English: hit-position relative to paddle center maps to
            // a ±45° angle.
            let center = self.paddle_x + PADDLE_W * 0.5;
            let off = ((self.ball_x - center) / (PADDLE_W * 0.5)).clamp(-1.0, 1.0);
            let angle = off * std::f32::consts::FRAC_PI_4;
            self.ball_vx = self.speed * angle.sin();
            self.ball_vy = -self.speed * angle.cos();
            self.paddle_squash = PADDLE_SQUASH_FRAMES;
        }

        // Bricks.
        let mut destroyed_count = 0u32;
        for i in 0..self.bricks.len() {
            if !self.bricks[i].alive {
                continue;
            }
            if let Some(side) = ball_brick_collision(
                self.ball_x,
                self.ball_y,
                BALL_R,
                &self.bricks[i],
            ) {
                // Bounce based on side hit.
                match side {
                    Side::Left => {
                        self.ball_x = self.bricks[i].x - BALL_R - 0.001;
                        self.ball_vx = -self.ball_vx.abs();
                    }
                    Side::Right => {
                        self.ball_x = self.bricks[i].x + self.bricks[i].w + BALL_R + 0.001;
                        self.ball_vx = self.ball_vx.abs();
                    }
                    Side::Top => {
                        self.ball_y = self.bricks[i].y - BALL_R - 0.001;
                        self.ball_vy = -self.ball_vy.abs();
                    }
                    Side::Bottom => {
                        self.ball_y = self.bricks[i].y + self.bricks[i].h + BALL_R + 0.001;
                        self.ball_vy = self.ball_vy.abs();
                    }
                }
                self.bricks[i].alive = false;
                self.pending_brick_deletes.push(self.bricks[i].id.clone());
                self.score += 10;
                self.score_dirty = true;
                destroyed_count += 1;
                self.spawn_sparks(&self.bricks[i].clone());
                self.speed = (self.speed + BALL_SPEED_PER_BRICK).min(BALL_MAX_SPEED);
                // Only one brick per tick (avoids weird multi-bounce).
                break;
            }
        }
        if destroyed_count > 0 && self.bricks.iter().all(|b| !b.alive) {
            self.state = GameState::Won;
        }

        // Game over: ball falls off.
        if self.ball_y - BALL_R > FIELD_H {
            self.state = GameState::Over;
        }
    }

    fn spawn_sparks(&mut self, brick: &Brick) {
        let cx = brick.x + brick.w * 0.5;
        let cy = brick.y + brick.h * 0.5;
        for i in 0..SPARKS_PER_BRICK {
            let theta = (i as f32) * (std::f32::consts::TAU / SPARKS_PER_BRICK as f32);
            let speed = 0.25 + 0.18 * pseudo_rand(self.next_spark_id as u32);
            let vx = theta.cos() * speed;
            let vy = theta.sin() * speed - 0.05;
            let id = format!("sp-{}", self.next_spark_id);
            self.next_spark_id += 1;
            self.sparks.push(Spark {
                id,
                x: cx,
                y: cy,
                vx,
                vy,
                color: brick.color,
                life: SPARK_LIFE,
                just_spawned: true,
                angle: pseudo_rand(self.next_spark_id as u32 ^ 0xA5A5) * std::f32::consts::TAU,
                spin: (pseudo_rand(self.next_spark_id as u32 ^ 0x5A5A) - 0.5)
                    * 2.0
                    * SPARK_MAX_SPIN,
            });
        }
    }
}

impl Clone for Brick {
    fn clone(&self) -> Self {
        Brick {
            id: self.id.clone(),
            x: self.x,
            y: self.y,
            w: self.w,
            h: self.h,
            color: self.color,
            alive: self.alive,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

/// Resolve ball-vs-brick collision. Returns the side of the brick that
/// was hit (the axis with smallest penetration), or None if no overlap.
fn ball_brick_collision(bx: f32, by: f32, br: f32, brick: &Brick) -> Option<Side> {
    let cx = bx.clamp(brick.x, brick.x + brick.w);
    let cy = by.clamp(brick.y, brick.y + brick.h);
    let dx = bx - cx;
    let dy = by - cy;
    if dx * dx + dy * dy > br * br {
        return None;
    }
    // Penetration depth on each axis.
    let pen_left = bx + br - brick.x;
    let pen_right = brick.x + brick.w - (bx - br);
    let pen_top = by + br - brick.y;
    let pen_bot = brick.y + brick.h - (by - br);
    let min = pen_left.min(pen_right).min(pen_top).min(pen_bot);
    if min == pen_left {
        Some(Side::Left)
    } else if min == pen_right {
        Some(Side::Right)
    } else if min == pen_top {
        Some(Side::Top)
    } else {
        Some(Side::Bottom)
    }
}

/// Tiny LCG for spark variety; deterministic + dependency-free.
fn pseudo_rand(seed: u32) -> f32 {
    let s = seed.wrapping_mul(2654435761).wrapping_add(0x9E3779B9);
    (s >> 8) as f32 / (1u32 << 24) as f32
}

fn color(rgba: u32) -> Color {
    let r = ((rgba >> 24) & 0xFF) as f32 / 255.0;
    let g = ((rgba >> 16) & 0xFF) as f32 / 255.0;
    let b = ((rgba >> 8) & 0xFF) as f32 / 255.0;
    let a = (rgba & 0xFF) as f32 / 255.0;
    Color { r, g, b, a }
}

// --- Drawing helpers ---

fn paddle_drawcmd() -> DrawCmd {
    DrawCmd::OutlineFillRectangles {
        fill: Style::Flat(color(0xE8_E8_F0_FF)),
        stroke: Style::Flat(color(0x6C_77_8B_FF)),
        line_width: 0.06,
        rects: vec![Rect {
            x: 0.0,
            y: 0.0,
            w: PADDLE_W,
            h: PADDLE_H,
        }],
    }
}

fn ball_drawcmd(cell_pw: f32, cell_ph: f32) -> DrawCmd {
    // VGE cells are anisotropic, so an arc with one radius draws a
    // slight ellipse on screen. To get a visually-circular ball we
    // use ArcEllipseTo with rx unchanged and ry compensated so
    // rx·cell_pw == ry·cell_ph. The path traverses CCW so that
    // femtovg's tessellator fills it cleanly.
    let rx = BALL_R;
    let ry = BALL_R * cell_pw / cell_ph;
    DrawCmd::FillPath {
        fill: Style::Flat(color(0xFE_E6_92_FF)),
        segments: vec![PathSegment {
            start: Point { x: -rx, y: 0.0 },
            nodes: vec![
                PathNode::ArcEllipseTo {
                    large: false,
                    sweep: false,
                    rx,
                    ry,
                    rotation: 0.0,
                    dst: Point { x: rx, y: 0.0 },
                },
                PathNode::ArcEllipseTo {
                    large: false,
                    sweep: false,
                    rx,
                    ry,
                    rotation: 0.0,
                    dst: Point { x: -rx, y: 0.0 },
                },
                PathNode::ClosePath,
            ],
        }],
    }
}

fn brick_drawcmd(b: &Brick, cell_pw: f32, cell_ph: f32) -> DrawCmd {
    let pad = BRICK_PAD;
    let x0 = pad;
    let y0 = pad;
    let x1 = b.w - pad;
    let y1 = b.h - pad;
    // rx in cell-width units; ry in cell-height units, compensated so
    // the corner pixel-radius is equal in both directions (i.e.
    // visually circular corners on anisotropic cell grids).
    let rx = BRICK_RADIUS.min((x1 - x0) * 0.45);
    let ry = (BRICK_RADIUS * cell_pw / cell_ph).min((y1 - y0) * 0.45);
    // Path traverses the rounded rect COUNTERCLOCKWISE (in y-down):
    // start on left edge, down → bottom → up right edge → top → close.
    // Each arc therefore sweeps CCW too (sweep=false). femtovg's
    // tessellator produced an upper-left notch when the path went CW
    // — `path.rect` (its built-in) uses CCW too.
    let arc = |dst: Point| PathNode::ArcEllipseTo {
        large: false,
        sweep: false,
        rx,
        ry,
        rotation: 0.0,
        dst,
    };
    DrawCmd::FillPath {
        fill: Style::Flat(b.color),
        segments: vec![PathSegment {
            start: Point { x: x0, y: y0 + ry },
            nodes: vec![
                PathNode::VerticalLineTo { y: y1 - ry },
                arc(Point { x: x0 + rx, y: y1 }),
                PathNode::HorizontalLineTo { x: x1 - rx },
                arc(Point { x: x1, y: y1 - ry }),
                PathNode::VerticalLineTo { y: y0 + ry },
                arc(Point { x: x1 - rx, y: y0 }),
                PathNode::HorizontalLineTo { x: x0 + rx },
                arc(Point { x: x0, y: y0 + ry }),
                PathNode::ClosePath,
            ],
        }],
    }
}

fn spark_drawcmd(c: Color) -> DrawCmd {
    DrawCmd::FillRectangles {
        fill: Style::Flat(c),
        rects: vec![Rect {
            x: -0.18,
            y: -0.18,
            w: 0.36,
            h: 0.36,
        }],
    }
}

// --- Terminal I/O ---

struct TtyGuard {
    fd: std::os::fd::RawFd,
    saved: Option<nix::sys::termios::Termios>,
    in_alt_screen: bool,
}

impl TtyGuard {
    fn enable() -> Result<Self> {
        use nix::sys::termios::{
            tcgetattr, tcsetattr, InputFlags, LocalFlags, OutputFlags, SetArg,
        };
        let stdin = std::io::stdin();
        let fd = stdin.as_raw_fd();
        let saved = tcgetattr(&stdin).context("tcgetattr")?;
        let mut raw = saved.clone();
        raw.local_flags &=
            !(LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ECHONL | LocalFlags::ISIG);
        raw.output_flags &= !OutputFlags::OPOST;
        raw.input_flags &= !(InputFlags::IXON
            | InputFlags::IXOFF
            | InputFlags::INLCR
            | InputFlags::ICRNL
            | InputFlags::IGNCR);
        tcsetattr(&stdin, SetArg::TCSANOW, &raw).context("tcsetattr (raw)")?;

        // Hide cursor.
        write_raw(b"\x1b[?25l")?;
        Ok(Self {
            fd,
            saved: Some(saved),
            in_alt_screen: false,
        })
    }

    fn enter_alt_screen(&mut self) -> Result<()> {
        write_raw(b"\x1b[?1049h")?;
        write_raw(b"\x1b[2J\x1b[H")?;
        self.in_alt_screen = true;
        Ok(())
    }
}

impl Drop for TtyGuard {
    fn drop(&mut self) {
        if self.in_alt_screen {
            // Leave alt screen — restores main screen content.
            let _ = write_raw(b"\x1b[?1049l");
        }
        let _ = write_raw(b"\x1b[?25h"); // cursor visible
        if let Some(saved) = self.saved.take() {
            use nix::sys::termios::{tcsetattr, SetArg};
            let _ = unsafe {
                let borrowed = std::os::fd::BorrowedFd::borrow_raw(self.fd);
                tcsetattr(borrowed, SetArg::TCSANOW, &saved)
            };
        }
    }
}

fn write_raw(bytes: &[u8]) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(bytes).context("stdout write")?;
    stdout.flush().context("stdout flush")?;
    Ok(())
}

fn send_and_drain(cmds: &[(Command, u32)], timeout: Duration) -> Result<()> {
    if cmds.is_empty() {
        return Ok(());
    }
    let env = build_envelope(cmds);
    write_raw(&env)?;
    let _ = drain_response_envelope(timeout)?;
    Ok(())
}

fn drain_response_envelope(timeout: Duration) -> Result<bool> {
    let mut apc = ApcStream::with_marker(*MARKER_T2C);
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 4096];
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(false);
        }
        if !poll_stdin_for(deadline - now)? {
            return Ok(false);
        }
        let n = read_stdin(&mut buf)?;
        if n == 0 {
            return Ok(false);
        }
        let out = apc.feed(&buf[..n]);
        if !out.payloads.is_empty() {
            return Ok(true);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ProbeData {
    cell_pixel_width: u16,
    cell_pixel_height: u16,
}

fn run_probe(timeout: Duration) -> Result<Option<ProbeData>> {
    let env = build_envelope(&[(Command::Probe, 0)]);
    write_raw(&env)?;
    let mut apc = ApcStream::with_marker(*MARKER_T2C);
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 4096];
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Ok(None);
        }
        if !poll_stdin_for(deadline - now)? {
            return Ok(None);
        }
        let n = read_stdin(&mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        let out = apc.feed(&buf[..n]);
        if !out.payloads.is_empty() {
            let payload = out.payloads.into_iter().next().unwrap();
            let mut r = Reader::new(&payload);
            let _ = r.u8(); // protocol_version
            let _ = r.u32(); // payload length
            let frame = r.u8().unwrap_or(0);
            if frame != RSP_PROBE {
                return Ok(None);
            }
            let _ = r.u32(); // request_id
            let _ = r.u32(); // body_len
            let _proto = r.u16().ok();
            let cw = r.u16().unwrap_or(9);
            let ch = r.u16().unwrap_or(20);
            return Ok(Some(ProbeData {
                cell_pixel_width: cw,
                cell_pixel_height: ch,
            }));
        }
    }
}

fn drain_stale_stdin() {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::fd::BorrowedFd;
    let fd = std::io::stdin().as_raw_fd();
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut buf = [0u8; 4096];
    loop {
        let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
        match poll(&mut fds, PollTimeout::ZERO) {
            Ok(n) if n > 0 => {
                if read_stdin(&mut buf).unwrap_or(0) == 0 {
                    break;
                }
            }
            _ => break,
        }
    }
}

fn poll_stdin_for(timeout: Duration) -> Result<bool> {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::fd::BorrowedFd;
    let ms: u16 = timeout.as_millis().min(i32::MAX as u128) as u16;
    let fd = std::io::stdin().as_raw_fd();
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
    let n = poll(&mut fds, PollTimeout::from(ms)).context("poll(stdin)")?;
    Ok(n > 0)
}

fn read_stdin(buf: &mut [u8]) -> Result<usize> {
    let fd = std::io::stdin().as_raw_fd();
    let n = nix::unistd::read(fd, buf).context("read(stdin)")?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brick(x: f32, y: f32) -> Brick {
        Brick {
            id: "b".into(),
            x,
            y,
            w: 4.0,
            h: 1.0,
            color: color(0),
            alive: true,
        }
    }

    #[test]
    fn collision_no_overlap() {
        let b = brick(10.0, 5.0);
        assert!(ball_brick_collision(0.0, 0.0, 0.5, &b).is_none());
    }

    #[test]
    fn collision_top_hit() {
        let b = brick(10.0, 5.0);
        let side = ball_brick_collision(11.0, 4.7, 0.5, &b).unwrap();
        assert!(matches!(side, Side::Top));
    }

    #[test]
    fn collision_left_hit() {
        let b = brick(10.0, 5.0);
        let side = ball_brick_collision(9.7, 5.4, 0.5, &b).unwrap();
        assert!(matches!(side, Side::Left));
    }

    #[test]
    fn brick_grid_count() {
        let g = Game::new(9.0, 20.0);
        assert_eq!(g.bricks.len(), BRICK_ROWS * BRICK_COLS);
    }

    #[test]
    fn paddle_clamps_to_field() {
        let mut g = Game::new(9.0, 20.0);
        g.move_paddle(-1000.0);
        assert!(g.paddle_x >= 0.0);
        g.move_paddle(1000.0);
        assert!(g.paddle_x + PADDLE_W <= FIELD_W + 1e-3);
    }

    #[test]
    fn ball_speed_grows_per_tick() {
        let mut g = Game::new(9.0, 20.0);
        let s0 = g.speed;
        g.step_physics();
        assert!(g.speed > s0);
    }
}
