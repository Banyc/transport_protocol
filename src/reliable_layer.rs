use std::{collections::VecDeque, num::NonZeroUsize, time::Instant};

use dre::{ConnectionState, PacketState};
use serde::{Deserialize, Serialize};
use strict_num::NonZeroPositiveF64;

use crate::{
    cont_act_timer::{ContActTimer, ContActTimer2, ContActTimerOn},
    packet_recv_space::PacketRecvSpace,
    packet_send_space::{PacketSendSpace, INIT_CWND},
    token_bucket::TokenBucket,
};

const SEND_DATA_BUFFER_LENGTH: usize = 2 << 16;
const RECV_DATA_BUFFER_LENGTH: usize = 2 << 16;
const MAX_BURST_PACKETS: usize = 64;
const MSS: usize = 1413;
const SMOOTH_SEND_RATE_ALPHA: f64 = 0.4;
const MIN_SEND_RATE: f64 = 16.;
const INIT_SEND_RATE: f64 = 128.;
const SEND_RATE_PROBE_RATE: f64 = 1.;
const CC_DATA_LOSS_RATE: f64 = 0.2;
const MAX_DATA_LOSS_RATE: f64 = 0.9;
const PRINT_DEBUG_MESSAGES: bool = false;
const SEND_DELIVERY_RATE_EPSILON: f64 = 0.1;

#[derive(Debug, Clone)]
enum State {
    Init {
        max_delivery_rate: Option<ContActTimer<NonZeroPositiveF64>>,
    },
    Fluctuating {
        max_delivery_rate: NonZeroPositiveF64,
        close_send_delivery_rate: ContActTimer2,
    },
}
impl State {
    pub fn init() -> Self {
        Self::Init {
            max_delivery_rate: None,
        }
    }

    pub fn fluctuating(max_delivery_rate: NonZeroPositiveF64, now: Instant) -> Self {
        Self::Fluctuating {
            max_delivery_rate,
            close_send_delivery_rate: ContActTimer2::new(now),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReliableLayer {
    send_data_buf: VecDeque<u8>,
    recv_data_buf: VecDeque<u8>,
    token_bucket: TokenBucket,
    connection_stats: ConnectionState,
    packet_send_space: PacketSendSpace,
    packet_recv_space: PacketRecvSpace,
    send_rate: NonZeroPositiveF64,
    state: State,
    prev_sample_rate: Option<dre::RateSample>,
    huge_data_loss_start: Option<Instant>,

    // Reused buffers
    packet_stats_buf: Vec<PacketState>,
    packet_buf: Vec<dre::Packet>,
}
impl ReliableLayer {
    pub fn new(now: Instant) -> Self {
        Self {
            send_data_buf: VecDeque::with_capacity(SEND_DATA_BUFFER_LENGTH),
            recv_data_buf: VecDeque::with_capacity(RECV_DATA_BUFFER_LENGTH),
            token_bucket: TokenBucket::new(
                NonZeroPositiveF64::new(INIT_SEND_RATE).unwrap(),
                NonZeroUsize::new(MAX_BURST_PACKETS).unwrap(),
                now,
            ),
            connection_stats: ConnectionState::new(now),
            packet_send_space: PacketSendSpace::new(),
            packet_recv_space: PacketRecvSpace::new(),
            send_rate: NonZeroPositiveF64::new(INIT_SEND_RATE).unwrap(),
            state: State::init(),
            prev_sample_rate: None,
            huge_data_loss_start: None,
            packet_stats_buf: Vec::new(),
            packet_buf: Vec::new(),
        }
    }

    pub fn packet_send_space(&self) -> &PacketSendSpace {
        &self.packet_send_space
    }

    pub fn token_bucket(&self) -> &TokenBucket {
        &self.token_bucket
    }

    /// Store data in the inner data buffer
    pub fn send_data_buf(&mut self, buf: &[u8], now: Instant) -> usize {
        self.detect_application_limited_phases(now);

        let free_bytes = self.send_data_buf.capacity() - self.send_data_buf.len();
        let write_bytes = free_bytes.min(buf.len());
        self.send_data_buf.extend(&buf[..write_bytes]);
        write_bytes
    }

    /// Move data from inner data buffer to inner packet space and return one of the packets if possible
    pub fn send_data_packet(&mut self, packet: &mut [u8], now: Instant) -> Option<DataPacket> {
        self.detect_application_limited_phases(now);

        // backoff on unrecovered huge data loss
        let mut f = || {
            let Some(data_loss_rate) = self.packet_send_space.data_loss_rate(now) else {
                self.huge_data_loss_start = None;
                return;
            };
            let huge_data_loss = INIT_CWND < self.packet_send_space.num_transmitting_packets()
                && MAX_DATA_LOSS_RATE < data_loss_rate;
            if !huge_data_loss {
                self.huge_data_loss_start = None;
                return;
            }
            if self.huge_data_loss_start.is_none() {
                self.huge_data_loss_start = Some(now);
            }
            let dur = now.duration_since(self.huge_data_loss_start.unwrap());
            if dur <= self.packet_send_space.rto_duration().mul_f64(2.) {
                return;
            }
            self.huge_data_loss_start = None;
            // exponential backoff
            let send_rate = NonZeroPositiveF64::new(self.send_rate.get() / 2.).unwrap();
            self.set_send_rate(send_rate, now);
            // Re-estimate the max delivery rate
            self.state = State::init();
        };
        f();

        if !self.token_bucket.take_exact_tokens(1, now) {
            return None;
        }

        if let Some(p) = self.packet_send_space.retransmit(now) {
            packet[..p.data.len()].copy_from_slice(p.data);

            return Some(DataPacket {
                seq: p.seq,
                data_written: NonZeroUsize::new(p.data.len()).unwrap(),
            });
        }

        // insufficient cwnd
        if !self.packet_send_space.accepts_new_packet() {
            return None;
        }

        let packet_bytes = packet.len().min(MSS).min(self.send_data_buf.len());
        let packet_bytes = NonZeroUsize::new(packet_bytes)?;

        let stats = self
            .connection_stats
            .send_packet_2(now, self.packet_send_space.no_packets_in_flight());

        let mut buf = self.packet_send_space.reuse_buf().unwrap_or_default();
        let data = self.send_data_buf.drain(..packet_bytes.get());
        buf.extend(data);
        let data = buf;

        packet[..data.len()].copy_from_slice(&data);
        let p = self.packet_send_space.send(data, stats, now);

        Some(DataPacket {
            seq: p.seq,
            data_written: packet_bytes,
        })
    }

    /// Take ACKs from the unreliable layer
    pub fn recv_ack_packet(&mut self, ack: &[u64], now: Instant) -> Option<dre::RateSample> {
        self.detect_application_limited_phases(now);

        self.packet_send_space
            .ack(ack, &mut self.packet_stats_buf, now);

        while let Some(p) = self.packet_stats_buf.pop() {
            self.packet_buf.push(dre::Packet {
                state: p,
                data_length: 1,
            })
        }
        let min_rtt = self.packet_send_space.min_rtt()?;
        let sr = self
            .connection_stats
            .sample_rate(&self.packet_buf, now, min_rtt);
        self.packet_stats_buf.clear();
        self.packet_buf.clear();

        let sr = sr?;
        if PRINT_DEBUG_MESSAGES {
            println!("{sr:?}");
        }
        self.prev_sample_rate = Some(sr.clone());

        self.adjust_send_rate(&sr, now);

        Some(sr)
    }

    fn adjust_send_rate(&mut self, sr: &dre::RateSample, now: Instant) {
        match &mut self.state {
            State::Init { max_delivery_rate } => {
                let Some(delivery_rate) = NonZeroPositiveF64::new(sr.delivery_rate()) else {
                    return;
                };
                let max_delivery_rate = match max_delivery_rate {
                    Some(x) => x,
                    None => {
                        *max_delivery_rate = Some(ContActTimer::new(
                            delivery_rate,
                            now,
                            ContActTimerOn::Unchanged,
                        ));
                        max_delivery_rate.as_mut().unwrap()
                    }
                };
                let rto = self.packet_send_space.rto_duration();
                let max_delivery_rate = max_delivery_rate.try_set_and_get(
                    |max| {
                        if *max < delivery_rate {
                            Some(delivery_rate)
                        } else {
                            None
                        }
                    },
                    rto.mul_f64(2.),
                    now,
                );
                if let Some(max_delivery_rate) = max_delivery_rate {
                    let send_rate = *max_delivery_rate;
                    self.state = State::fluctuating(*max_delivery_rate, now);
                    self.set_send_rate(send_rate, now);
                    self.adjust_send_rate(sr, now);
                    return;
                }

                // exponential growth
                let send_rate = NonZeroPositiveF64::new(delivery_rate.get() * 2.).unwrap();
                self.set_send_rate(send_rate, now);
            }
            State::Fluctuating {
                max_delivery_rate,
                close_send_delivery_rate,
            } => {
                let little_data_loss = self
                    .packet_send_space
                    .data_loss_rate(now)
                    .map(|lr| lr < CC_DATA_LOSS_RATE);
                let should_probe = little_data_loss != Some(false);
                let target_send_rate = match should_probe {
                    true => {
                        let send_rate =
                            sr.delivery_rate() + sr.delivery_rate() * SEND_RATE_PROBE_RATE;
                        if send_rate < self.send_rate.get() {
                            return;
                        }
                        send_rate.min(max_delivery_rate.get())
                    }
                    false => sr.delivery_rate(),
                };

                let smooth_send_rate = self.send_rate.get() * (1. - SMOOTH_SEND_RATE_ALPHA)
                    + target_send_rate * SMOOTH_SEND_RATE_ALPHA;
                let send_rate = NonZeroPositiveF64::new(smooth_send_rate).unwrap();

                // Re-estimate the max delivery rate
                let few_data_to_send = self.send_data_buf.len() <= MSS
                    && self.packet_send_space.no_packets_in_flight();
                let close_send_delivery_rate_ = send_rate.get()
                    < sr.delivery_rate() + sr.delivery_rate() * SEND_DELIVERY_RATE_EPSILON
                    && send_rate == *max_delivery_rate;
                let good_delivery_rate = close_send_delivery_rate.check(
                    close_send_delivery_rate_,
                    self.packet_send_space.rto_duration(),
                    now,
                );
                if few_data_to_send || good_delivery_rate {
                    self.state = State::init();
                }

                self.set_send_rate(send_rate, now);
            }
        }
    }

    /// Return data from the inner data buffer and inner packet space
    pub fn recv_data_buf(&mut self, buf: &mut [u8]) -> usize {
        let read_bytes = buf.len().min(self.recv_data_buf.len());
        let (a, b) = self.recv_data_buf.as_slices();
        let n_a = a.len().min(read_bytes);
        let n_b = read_bytes - n_a;
        buf[..n_a].copy_from_slice(&a[..n_a]);
        buf[n_a..read_bytes].copy_from_slice(&b[..n_b]);
        self.recv_data_buf.drain(..read_bytes);
        self.move_recv_data();
        read_bytes
    }

    /// Take a packet from the unreliable layer
    ///
    /// Return `false` if the data is rejected due to window capacity
    pub fn recv_data_packet(&mut self, seq: u64, packet: &[u8]) -> bool {
        let mut buf = self.packet_recv_space.reuse_buf().unwrap_or_default();
        buf.extend(packet);
        if !self.packet_recv_space.recv(seq, buf) {
            return false;
        }
        self.move_recv_data();
        true
    }

    /// Move data from packet space to data buffer
    fn move_recv_data(&mut self) {
        while let Some(p) = self.packet_recv_space.peak() {
            if self.recv_data_buf.capacity() - self.recv_data_buf.len() < p.len() {
                return;
            }
            let p = self.packet_recv_space.pop().unwrap();
            self.recv_data_buf.extend(&p);
            self.packet_recv_space.return_buf(p);
        }
    }

    fn detect_application_limited_phases(&mut self, now: Instant) {
        self.connection_stats.detect_application_limited_phases_2(
            dre::DetectAppLimitedPhaseParams {
                few_data_to_send: self.send_data_buf.len() < MSS,
                not_transmitting_a_packet: true,
                cwnd_not_full: self.packet_send_space.accepts_new_packet(),
                all_lost_packets_retransmitted: self
                    .packet_send_space
                    .all_lost_packets_retransmitted(now),
                pipe: self
                    .packet_send_space
                    .num_not_lost_transmitting_packets(now) as u64,
            },
        );
    }

    fn set_send_rate(&mut self, send_rate: NonZeroPositiveF64, now: Instant) {
        let send_rate = NonZeroPositiveF64::new(MIN_SEND_RATE)
            .unwrap()
            .max(send_rate);
        self.send_rate = send_rate;
        self.packet_send_space.set_send_rate(send_rate);
        self.token_bucket.set_thruput(send_rate, now);
    }

    pub fn log(&self) -> Log {
        let now = Instant::now();
        let min_rtt = self.packet_send_space.min_rtt();
        Log {
            tokens: self.token_bucket.outdated_tokens(),
            send_rate: self.send_rate.get(),
            loss_rate: self.packet_send_space.data_loss_rate(now),
            num_tx_pkts: self.packet_send_space.num_transmitting_packets(),
            num_rt_pkts: self.packet_send_space.num_retransmitted_packets(),
            send_seq: self.packet_send_space.next_seq(),
            min_rtt: min_rtt.map(|t| t.as_millis()),
            rtt: self.packet_send_space.smooth_rtt().as_millis(),
            cwnd: self.packet_send_space.cwnd().get(),
            num_rx_pkts: self.packet_recv_space.num_received_packets(),
            recv_seq: self.packet_recv_space.next_seq(),
            delivery_rate: self.prev_sample_rate.as_ref().map(|sr| sr.delivery_rate()),
            state: match &self.state {
                State::Init { .. } => 0,
                State::Fluctuating { .. } => 1,
            },
            app_limited: self.prev_sample_rate.as_ref().map(|sr| sr.is_app_limited()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DataPacket {
    pub seq: u64,
    pub data_written: NonZeroUsize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Log {
    pub tokens: f64,
    pub send_rate: f64,
    pub loss_rate: Option<f64>,
    pub num_tx_pkts: usize,
    pub num_rt_pkts: usize,
    pub send_seq: u64,
    pub min_rtt: Option<u128>,
    pub rtt: u128,
    pub cwnd: usize,
    pub num_rx_pkts: usize,
    pub recv_seq: u64,
    pub delivery_rate: Option<f64>,
    pub state: u8,
    pub app_limited: Option<bool>,
}
