//! Unified event handler — multiplexes terminal, gateway, and timer events.

use std::time::Duration;

use crossterm::event::EventStream;
use futures::StreamExt;
use tokio::sync::mpsc;
use workforce_gateway_client::GatewayEvent;

/// Events consumed by the main TUI loop.
#[derive(Debug)]
pub enum Event {
    /// A crossterm terminal event (key press, resize, etc.).
    Terminal(crossterm::event::Event),
    /// A gateway WebSocket event.
    Gateway(GatewayEvent),
    /// Periodic tick for animations (spinners, dots).
    Tick,
    /// Time to render a frame.
    Render,
}

/// Spawns a background task that funnels all event sources into a single channel.
pub struct EventHandler {
    rx: mpsc::UnboundedReceiver<Event>,
    _task: tokio::task::JoinHandle<()>,
}

impl EventHandler {
    /// Create a new event handler.
    ///
    /// - `gateway_rx`: the receiver from `GatewayClient::take_events()`
    /// - `tick_rate`: how often to fire `Tick` events (Hz)
    /// - `frame_rate`: how often to fire `Render` events (fps)
    pub fn new(gateway_rx: mpsc::Receiver<GatewayEvent>, tick_rate: f64, frame_rate: f64) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let tick_interval = Duration::from_secs_f64(1.0 / tick_rate);
        let render_interval = Duration::from_secs_f64(1.0 / frame_rate);

        let task = tokio::spawn(Self::event_loop(
            tx,
            gateway_rx,
            tick_interval,
            render_interval,
        ));

        Self { rx, _task: task }
    }

    /// Get the next event, blocking until one is available.
    pub async fn next(&mut self) -> Option<Event> {
        self.rx.recv().await
    }

    async fn event_loop(
        tx: mpsc::UnboundedSender<Event>,
        mut gateway_rx: mpsc::Receiver<GatewayEvent>,
        tick_interval: Duration,
        render_interval: Duration,
    ) {
        let mut crossterm_events = EventStream::new();
        let mut tick_timer = tokio::time::interval(tick_interval);
        let mut render_timer = tokio::time::interval(render_interval);
        let mut gateway_alive = true;

        loop {
            tokio::select! {
                // Terminal input (keyboard, mouse, resize)
                maybe = crossterm_events.next() => {
                    if let Some(Ok(event)) = maybe {
                        if tx.send(Event::Terminal(event)).is_err() {
                            break;
                        }
                    }
                }
                // Gateway WebSocket events
                maybe = gateway_rx.recv(), if gateway_alive => {
                    match maybe {
                        Some(gw_event) => {
                            if tx.send(Event::Gateway(gw_event)).is_err() {
                                break;
                            }
                        }
                        None => {
                            // Gateway channel closed — notify TUI but keep running
                            let _ = tx.send(Event::Gateway(GatewayEvent::Disconnected {
                                reason: "Gateway shut down".to_string(),
                            }));
                            gateway_alive = false;
                        }
                    }
                }
                // Tick for animations
                _ = tick_timer.tick() => {
                    if tx.send(Event::Tick).is_err() {
                        break;
                    }
                }
                // Render trigger
                _ = render_timer.tick() => {
                    if tx.send(Event::Render).is_err() {
                        break;
                    }
                }
            }
        }
    }
}
