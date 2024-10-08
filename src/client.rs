use std::{net::SocketAddr, time::Duration};

use bytes::{Buf, BufMut, Bytes, BytesMut};
use color_eyre::eyre::eyre;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};
use tracing::{debug, field, trace, trace_span, warn, Instrument, Span};

enum State {
    InitialConnection,
    ReceivingPassword {
        version: String,
    },
    ReveivingInfo {
        version: String,
        password: Option<String>,
        name: Option<String>,
        uuid: Option<String>,
    },
}

fn check_zero_remaining(source: &Bytes) {
    if !source.is_empty() {
        warn!(
            "Finished reading packet but didn't reach end of body.\n\
            \tremaining: {source:?}"
        );
    }
}

fn get_length_prefixed_bytes(source: &mut impl Buf) -> Bytes {
    let length = source.get_u8();
    source.copy_to_bytes(length as _)
}

async fn read_timeout<R>(
    duration: Duration,
    reader: &mut R,
    dest: &mut [u8],
) -> std::io::Result<usize>
where
    R: Unpin,
    R: AsyncRead,
{
    tokio::time::timeout(duration, reader.read(dest))
        .instrument(trace_span!("read"))
        .await?
}

async fn write_all_timeout<W>(writer: &mut W, src: &[u8]) -> std::io::Result<()>
where
    W: Unpin,
    W: AsyncWrite,
{
    let mut read = 0;
    while read < src.len() {
        read += tokio::time::timeout(crate::IDLE_TIMEOUT, writer.write(&src[read..]))
            .instrument(trace_span!("write"))
            .await??;
    }

    std::result::Result::Ok(())
}

pub async fn handle_client(
    stream: TcpStream,
    _peer_addr: SocketAddr,
    password_chance: f32,
) -> std::io::Result<(String, Option<String>, String, String)> {
    let (mut client_reader, mut client_writer) = stream.into_split();

    // not that happy with this, may come back to it
    let mut connection_state = State::InitialConnection;

    let mut read_buf = vec![0; 64];
    let mut decode_buf = BytesMut::new();

    loop {
        async {
            // give the client a little more time if they're at the password stage
            let timeout_duration = if let State::ReceivingPassword { .. } = &connection_state {
                // todo: need to tune this
                Duration::from_secs(30)
            } else {
                crate::IDLE_TIMEOUT
            };

            let len = read_timeout(timeout_duration, &mut client_reader, &mut read_buf).await?;
            if len == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
            }

            decode_buf.put_slice(&read_buf[..len]);

            // if we're receiving more than this before having a valid packet,
            // there's potentially something funky going on
            if decode_buf.len() >= crate::MAX_BUFFER_LENGTH {
                warn!(
                    "Received {} bytes with no packet, disconnecting.",
                    decode_buf.len()
                );

                return Err(std::io::Error::other(eyre!("Buffer to large")));
            }

            Ok(())
        }
        .instrument(trace_span!("client.read"))
        .await?;

        if decode_buf.len() < 2 {
            continue;
        }

        let mut packet_buf = decode_buf.clone();

        let packet_length = packet_buf.get_u16_le() as usize;
        if packet_length < 3 {
            return Err(std::io::Error::other(eyre!("Invalid packet length")));
        }

        // subtract length of the length from the length :)))))))
        let data_length = packet_length - 2;

        // if we have enough data to read the full packet then split it of from the decode buffer and do that
        if packet_buf.len() >= data_length {
            let mut body = packet_buf.split_to(data_length).freeze();
            // essentialy removes the current packet from the decude buffer
            std::mem::swap(&mut packet_buf, &mut decode_buf);

            let id = body.get_i8();
            trace!("> packet ${id:02x}: {body:?}");

            connection_state = match (id, connection_state) {
                (0x01, State::InitialConnection) => {
                    async {
                        let signature = get_length_prefixed_bytes(&mut body);
                        let signature = String::from_utf8_lossy(&signature);
                        Span::current().record("signature", &*signature);

                        check_zero_remaining(&body);

                        if let Some((_, version)) = signature.split_once("Terraria") {
                            debug!("> ConnectRequest(version: {version})");

                            if password_chance > fastrand::f32() {
                                // write RequestPassword packet
                                write_all_timeout(&mut client_writer, b"\x03\x00\x25")
                                    .instrument(trace_span!(
                                        "client.write",
                                        packet = "RequestPassword"
                                    ))
                                    .await?;

                                Ok(State::ReceivingPassword {
                                    version: version.to_string(),
                                })
                            } else {
                                // write ContinueConnecting packet with a 0 player id
                                write_all_timeout(&mut client_writer, b"\x05\x00\x03\0\0")
                                    .instrument(trace_span!(
                                        "client.write",
                                        packet = "ContinueConnecting(0)"
                                    ))
                                    .await?;

                                Ok(State::ReveivingInfo {
                                    version: version.to_string(),
                                    password: None,
                                    name: None,
                                    uuid: None,
                                })
                            }
                        } else {
                            warn!("> Unknown ConnectRequest signature: {signature:?}");
                            Err(std::io::Error::other(eyre!("Unknown signature")))
                        }
                    }
                    .instrument(trace_span!(
                        "client.handle_packet",
                        packet = "ConnectRequest",
                        signature = field::Empty
                    ))
                    .await?
                }

                (0x26, State::ReceivingPassword { version }) => {
                    async {
                        let password = get_length_prefixed_bytes(&mut body);
                        let password = String::from_utf8_lossy(&password);
                        Span::current().record("password", &*password);

                        check_zero_remaining(&body);

                        debug!("> SendPassword(password: {password:?})");

                        // write ContinueConnecting packet with a 0 player id
                        write_all_timeout(&mut client_writer, b"\x05\x00\x03\0\0")
                            .instrument(trace_span!(
                                "client.write",
                                packet = "ContinueConnecting(0)"
                            ))
                            .await?;

                        std::io::Result::Ok(State::ReveivingInfo {
                            version,
                            password: Some(password.to_string()),
                            name: None,
                            uuid: None,
                        })
                    }
                    .instrument(trace_span!(
                        "client.handle_packet",
                        packet = "SendPassword",
                        password = field::Empty
                    ))
                    .await?
                }

                (
                    0x04,
                    State::ReveivingInfo {
                        version,
                        password,
                        name: _,
                        uuid,
                    },
                ) => {
                    async {
                        let _ = body.get_u8();
                        let _ = body.get_u8();
                        let _ = body.get_u8();

                        let name = get_length_prefixed_bytes(&mut body);
                        let name = String::from_utf8_lossy(&name);
                        Span::current().record("player_name", &*name);

                        // not reading the whole packet, there will definately be bytes left over

                        debug!("> PlayerInfo(name: {name:?}");

                        State::ReveivingInfo {
                            version,
                            password,
                            name: Some(name.to_string()),
                            uuid,
                        }
                    }
                    .instrument(trace_span!(
                        "client.handle_packet",
                        packet = "PlayerInfo",
                        player_name = field::Empty
                    ))
                    .await
                }

                (
                    0x44,
                    State::ReveivingInfo {
                        version,
                        password,
                        name,
                        uuid: _,
                    },
                ) => {
                    async {
                        let uuid = get_length_prefixed_bytes(&mut body);
                        let uuid = String::from_utf8_lossy(&uuid);
                        Span::current().record("player_uuid", &*uuid);

                        check_zero_remaining(&body);

                        debug!("> ClientUUID(uuid: {uuid:?})");

                        State::ReveivingInfo {
                            version,
                            password,
                            name,
                            uuid: Some(uuid.to_string()),
                        }
                    }
                    .instrument(trace_span!(
                        "client.handle_packet",
                        packet = "ClientUUID",
                        player_uuid = field::Empty
                    ))
                    .await
                }

                // don't really care that much about the information other packets can give
                (_, state) => state,
            };

            if let State::ReveivingInfo {
                version,
                password,
                name: Some(name),
                uuid: Some(uuid),
            } = connection_state
            {
                Span::current()
                    .record("version", &version)
                    .record("password", &password)
                    .record("player_name", &name)
                    .record("player_uuid", &uuid);

                return Ok((version, password, name, uuid));
            }
        }
    }
}
