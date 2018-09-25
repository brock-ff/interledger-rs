mod ilp_packet_stream;
mod packet;
mod packet_stream;
mod request_id_checker;
mod client;

pub use self::ilp_packet_stream::IlpPacketStream;
pub use self::packet::{
  deserialize_packet, BtpError, BtpMessage, BtpPacket, BtpResponse, ContentType, ProtocolData,
  Serializable,
};
pub use self::packet_stream::BtpPacketStream;
pub use self::request_id_checker::BtpRequestIdCheckerStream;
pub use errors::ParseError;
pub use self::client::{ClientPlugin, connect_async};

use futures::Future;
use base64;
use ring::rand::{SecureRandom, SystemRandom};

pub fn connect_to_moneyd() -> impl Future<Item = ClientPlugin, Error = ()> {
  let url = format!("ws://{}:{}@localhost:7768", random_token(), random_token());
  connect_async(&url)
}

fn random_token() -> String {
  let mut bytes: [u8; 32] = [0; 32];
  SystemRandom::new().fill(&mut bytes).unwrap();
  base64::encode_config(&bytes, base64::URL_SAFE_NO_PAD)
}
