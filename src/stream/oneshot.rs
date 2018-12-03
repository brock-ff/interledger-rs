use super::congestion::CongestionController;
use super::crypto::{generate_condition, generate_fulfillment, random_u32};
use super::packet::*;
use super::Error;
use bytes::{Bytes, BytesMut};
use chrono::{Duration, Utc};
use crate::ildcp::{IldcpRequest, IldcpResponse};
use crate::ilp::{
    parse_f08_error, IlpFulfill, IlpPacket, IlpPrepare, IlpReject, PacketType as IlpPacketType,
};
use crate::plugin::{IlpRequest, Plugin};
use futures::{Async, AsyncSink, Future, Poll};
use num_bigint::BigUint;
use num_traits::{One, Zero};
use std::cmp::min;
use std::collections::HashMap;

pub fn send_money<S, T, U>(
    plugin: S,
    destination_account: T,
    shared_secret: U,
    source_amount: u64,
) -> impl Future<Item = (u64, S), Error = Error>
where
    S: Plugin<Item = IlpRequest, Error = (), SinkItem = IlpRequest, SinkError = ()> + 'static,
    String: From<T>,
    Bytes: From<U>,
{
    SendMoneyFuture {
        state: SendMoneyFutureState::NeedIldcp,
        plugin: Some(plugin),
        source_account: None,
        destination_account: String::from(destination_account),
        shared_secret: Bytes::from(shared_secret),
        source_amount,
        congestion_controller: CongestionController::default(),
        outgoing_request: None,
        pending_prepares: HashMap::new(),
        amount_delivered: 0,
        should_send_source_account: true,
        sequence: 1,
    }
}

struct SendMoneyFuture<S> {
    state: SendMoneyFutureState,
    plugin: Option<S>,
    source_account: Option<String>,
    destination_account: String,
    shared_secret: Bytes,
    source_amount: u64,
    congestion_controller: CongestionController,
    outgoing_request: Option<IlpRequest>,
    pending_prepares: HashMap<u32, IlpPrepare>,
    amount_delivered: u64,
    should_send_source_account: bool,
    sequence: u64,
}

#[derive(PartialEq)]
enum SendMoneyFutureState {
    NeedIldcp,
    SentIldcpRequest,
    SendMoney,
}

impl<S> SendMoneyFuture<S>
where
    S: Plugin<Item = IlpRequest, Error = (), SinkItem = IlpRequest, SinkError = ()> + 'static,
{
    fn try_send_money(&mut self) -> Result<(), Error> {
        // Determine the amount to send
        let amount = min(
            self.source_amount,
            self.congestion_controller.get_max_amount(),
        );
        if amount == 0 {
            return Ok(());
        }
        self.source_amount -= amount;

        // Load up the STREAM packet
        let mut frames = vec![Frame::StreamMoney(StreamMoneyFrame {
            stream_id: BigUint::one(),
            shares: BigUint::one(),
        })];
        if self.should_send_source_account {
            if let Some(ref source_account) = self.source_account {
                frames.push(Frame::ConnectionNewAddress(ConnectionNewAddressFrame {
                    source_account: source_account.to_string(),
                }));
            }
        }
        let stream_packet = StreamPacket {
            ilp_packet_type: IlpPacketType::IlpPrepare,
            // TODO enforce min exchange rate
            prepare_amount: 0,
            sequence: self.next_sequence(),
            frames,
        };

        // Create the ILP Prepare packet
        let data = stream_packet.to_encrypted(&self.shared_secret).unwrap();
        let execution_condition = generate_condition(&self.shared_secret, &data);
        let prepare = IlpPrepare::new(
            self.destination_account.to_string(),
            amount,
            execution_condition,
            Utc::now() + Duration::seconds(30),
            data,
        );

        // Send it!
        let request_id = random_u32();
        debug!(
            "Sending request {} with amount: {} and encrypted STREAM packet: {:?}",
            request_id, amount, stream_packet
        );
        // TODO don't copy prepare packet
        self.pending_prepares.insert(request_id, prepare.clone());
        self.try_send_outgoing((request_id, IlpPacket::Prepare(prepare)))?;
        Ok(())
    }

    // Either send the outgoing request or store it as pending so it can be sent later
    fn try_send_outgoing(&mut self, request: IlpRequest) -> Poll<(), Error> {
        if let Some(ref mut plugin) = self.plugin {
            match plugin.start_send(request) {
                Ok(AsyncSink::NotReady(request)) => {
                    self.outgoing_request = Some(request);
                    Ok(Async::NotReady)
                }
                Ok(AsyncSink::Ready) => Ok(Async::Ready(())),
                Err(_) => Err(Error::ConnectionError(
                    "Unable to send request to plugin".to_string(),
                )),
            }
        } else {
            panic!("Polled after finish");
        }
    }

    fn handle_incoming(&mut self) -> Result<(), Error> {
        loop {
            let next = {
                if let Some(ref mut plugin) = self.plugin {
                    plugin.poll()
                } else {
                    panic!("Poll after finish");
                }
            };
            match next {
                Ok(Async::NotReady) => {
                    return Ok(());
                }
                Ok(Async::Ready(None)) => {
                    return Err(Error::ConnectionError(
                        "Plugin closed before amount was sent".to_string(),
                    ));
                }
                Err(_) => {
                    return Err(Error::ConnectionError(
                        "Error polling plugin for more packets".to_string(),
                    ));
                }
                Ok(Async::Ready(Some((id, ilp_packet)))) => match ilp_packet {
                    IlpPacket::Prepare(ref prepare) => self.handle_prepare(id, &prepare)?,
                    IlpPacket::Fulfill(ref fulfill) => self.handle_fulfill(id, &fulfill),
                    IlpPacket::Reject(ref reject) => self.handle_reject(id, &reject),
                },
            }
        }
    }

    fn handle_prepare(&mut self, id: u32, prepare: &IlpPrepare) -> Result<(), Error> {
        let source_account = if let Some(ref source_account) = self.source_account {
            source_account.to_string()
        } else {
            String::new()
        };
        if let Ok(request_packet) =
            StreamPacket::from_encrypted(&self.shared_secret, BytesMut::from(&prepare.data[..]))
        {
            if prepare.amount == 0 {
                let packet = StreamPacket {
                    ilp_packet_type: IlpPacketType::IlpFulfill,
                    prepare_amount: prepare.amount,
                    sequence: request_packet.sequence,
                    frames: Vec::new(),
                };
                let data = packet.to_encrypted(&self.shared_secret).unwrap();
                let fulfillment = generate_fulfillment(&self.shared_secret, &data);
                self.try_send_outgoing((
                    id,
                    IlpPacket::Fulfill(IlpFulfill::new(fulfillment, data)),
                ))?;
            } else {
                // Tell the sender we don't want to receive money
                let mut frames = Vec::new();
                for frame in request_packet.frames {
                    if let Frame::StreamMoney(StreamMoneyFrame {
                        stream_id,
                        shares: _shares,
                    }) = frame
                    {
                        frames.push(Frame::StreamMaxMoney(StreamMaxMoneyFrame {
                            stream_id,
                            receive_max: BigUint::zero(),
                            total_received: BigUint::zero(),
                        }));
                    }
                }
                let packet = StreamPacket {
                    ilp_packet_type: IlpPacketType::IlpReject,
                    prepare_amount: prepare.amount,
                    sequence: request_packet.sequence,
                    frames,
                };
                let data = packet.to_encrypted(&self.shared_secret).unwrap();
                self.try_send_outgoing((
                    id,
                    IlpPacket::Reject(IlpReject::new("F99", String::new(), source_account, data)),
                ))?;
            }
        } else {
            self.try_send_outgoing((
                id,
                IlpPacket::Reject(IlpReject::new(
                    "F06",
                    String::new(),
                    source_account,
                    Bytes::new(),
                )),
            ))?;
        }

        Ok(())
    }

    fn handle_fulfill(&mut self, id: u32, fulfill: &IlpFulfill) {
        if self.state == SendMoneyFutureState::SentIldcpRequest {
            if let Ok(response) = IldcpResponse::from_fulfill(fulfill) {
                debug!("Got ILDCP response: {:?}", response);
                self.source_account = Some(response.client_address.to_string());
                self.state = SendMoneyFutureState::SendMoney;
                return;
            }
        }

        if let Some(prepare) = self.pending_prepares.remove(&id) {
            // TODO should we check the fulfillment and expiry or can we assume the plugin does that?
            self.congestion_controller.fulfill(id);
            self.should_send_source_account = false;

            if let Ok(packet) = StreamPacket::from_encrypted(
                &self.shared_secret,
                BytesMut::from(fulfill.data.clone()),
            ) {
                if packet.ilp_packet_type == IlpPacketType::IlpFulfill {
                    // TODO check that the sequence matches our outgoing packet
                    self.amount_delivered += packet.prepare_amount;
                }
            }

            debug!(
                "Prepare {} with amount {} was fulfilled ({} left to send)",
                id, prepare.amount, self.source_amount
            );
        } else {
            warn!(
                "Got unexpected fulfill packet with id {}: {:?}",
                id, fulfill
            );
        }
    }

    fn handle_reject(&mut self, id: u32, reject: &IlpReject) {
        if let Some(prepare) = self.pending_prepares.remove(&id) {
            self.source_amount += prepare.amount;
            self.congestion_controller.reject(id, &reject.code);
            // Handle F08 errors, which communicate the maximum packet amount
            if let Some(err_details) = parse_f08_error(&reject) {
                let max_packet_amount: u64 =
                    prepare.amount * err_details.max_amount / err_details.amount_received;
                self.congestion_controller
                    .set_max_packet_amount(max_packet_amount);
            }
            debug!(
                "Prepare {} with amount {} was rejected with code: {} ({} left to send)",
                id, prepare.amount, &reject.code, self.source_amount
            );
        }
    }

    fn next_sequence(&mut self) -> u64 {
        let seq = self.sequence;
        self.sequence += 1;
        seq
    }
}

impl<S> Future for SendMoneyFuture<S>
where
    S: Plugin<Item = IlpRequest, Error = (), SinkItem = IlpRequest, SinkError = ()> + 'static,
{
    type Item = (u64, S);
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self.plugin.is_none() {
            return Err(Error::PollError(
                "Attempted to poll after future was finished".to_string(),
            ));
        }

        // Get the ILDCP details first
        if self.state == SendMoneyFutureState::NeedIldcp {
            let ildcp_request = IlpPacket::Prepare(IldcpRequest::new().to_prepare());
            self.state = SendMoneyFutureState::SentIldcpRequest;
            try_ready!(self.try_send_outgoing((random_u32(), ildcp_request)))
        }

        // Try sending the buffered request
        if let Some(request) = self.outgoing_request.take() {
            try_ready!(self.try_send_outgoing(request))
        }

        // Check for incoming packets
        self.handle_incoming()?;

        // Check if we're still waiting on the ILDCP response
        if self.state != SendMoneyFutureState::SendMoney {
            return Ok(Async::NotReady);
        }

        if self.source_amount == 0 && self.pending_prepares.is_empty() {
            Ok(Async::Ready((
                self.amount_delivered,
                self.plugin.take().unwrap(),
            )))
        } else {
            self.try_send_money()?;
            Ok(Async::NotReady)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::mock::create_mock_plugins;

    mod send_money {
        use super::*;
        use crate::stream::crypto::random_condition;
        use crate::stream::StreamListener;
        use env_logger;
        use futures::Stream;
        use tokio::runtime::current_thread::Runtime;

        #[test]
        fn send_to_normal_listener() {
            env_logger::init();
            let (sender, receiver) = create_mock_plugins();
            let mut runtime = Runtime::new().unwrap();
            let server_secret = random_condition();
            let run = StreamListener::bind(receiver, server_secret.clone())
                .and_then(|(listener, conn_generator)| {
                    let handle_connections = listener.for_each(|(_id, conn)| {
                        let handle_streams = conn.for_each(|stream| {
                            let handle_money = stream.money.for_each(|amount| {
                                debug!("Got money: {}", amount);
                                Ok(())
                            });
                            tokio::spawn(handle_money);
                            Ok(())
                        });
                        tokio::spawn(handle_streams);
                        Ok(())
                    });
                    tokio::spawn(handle_connections);

                    let (destination_account, shared_secret) =
                        conn_generator.generate_address_and_secret("test");
                    send_money(sender, destination_account, shared_secret, 3000).and_then(
                        |(amount_delivered, _plugin)| {
                            assert_eq!(amount_delivered, 3000);
                            Ok(())
                        },
                    )
                }).map_err(|err| panic!(err));

            runtime.block_on(run).unwrap();
        }
    }
}
