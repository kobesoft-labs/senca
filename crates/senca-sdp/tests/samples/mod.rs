#![allow(dead_code)]
//! 試験で使う SDP の見本。

/// RFC 8866 5 の例。
pub const RFC8866: &str = "v=0\r
o=jdoe 3724394400 3724394405 IN IP4 198.51.100.1\r
s=Call to John Smith\r
i=SDP Offer #1\r
u=http://www.jdoe.example.com/home.html\r
e=Jane Doe <jane@jdoe.example.com>\r
p=+1 617 555-6011\r
c=IN IP4 198.51.100.1\r
t=0 0\r
m=audio 49170 RTP/AVP 0\r
m=audio 49180 RTP/AVP 0\r
m=video 51372 RTP/AVP 99\r
c=IN IP6 2001:db8::2\r
a=rtpmap:99 h263-1998/90000\r
";

/// RFC 4317 2.1 の Alice の offer（s= は空白 1 つ）。
pub const RFC4317_OFFER: &str = "v=0\r
o=alice 2890844526 2890844526 IN IP4 host.atlanta.example.com\r
s= \r
c=IN IP4 host.atlanta.example.com\r
t=0 0\r
m=audio 49170 RTP/AVP 0 8 97\r
a=rtpmap:0 PCMU/8000\r
a=rtpmap:8 PCMA/8000\r
a=rtpmap:97 iLBC/8000\r
m=video 51372 RTP/AVP 31 32\r
a=rtpmap:31 H261/90000\r
a=rtpmap:32 MPV/90000\r
";

/// RFC 4317 2.1 の Bob の answer。
pub const RFC4317_ANSWER: &str = "v=0\r
o=bob 2808844564 2808844564 IN IP4 host.biloxi.example.com\r
s= \r
c=IN IP4 host.biloxi.example.com\r
t=0 0\r
m=audio 49174 RTP/AVP 0\r
a=rtpmap:0 PCMU/8000\r
m=video 49170 RTP/AVP 32\r
a=rtpmap:32 MPV/90000\r
";

/// Chrome の offer（音声 + 映像 simulcast + DataChannel、BUNDLE）。
pub const CHROME_OFFER: &str = "v=0\r
o=- 4611731400430051336 2 IN IP4 127.0.0.1\r
s=-\r
t=0 0\r
a=group:BUNDLE 0 1 2\r
a=extmap-allow-mixed\r
a=msid-semantic: WMS stream\r
m=audio 9 UDP/TLS/RTP/SAVPF 111 63 9 0 8 13 110 126\r
c=IN IP4 0.0.0.0\r
a=rtcp:9 IN IP4 0.0.0.0\r
a=candidate:1467250027 1 udp 2122260223 192.168.0.196 46243 typ host generation 0 network-id 1\r
a=candidate:2 1 udp 2113937151 0f3e2a8c-6c2b-4bd0-a5ac-6f3e9e8b2b4a.local 54321 typ host generation 0\r
a=candidate:3 1 udp 1686052607 203.0.113.7 46243 typ srflx raddr 192.168.0.196 rport 46243 generation 0\r
a=candidate:4 1 tcp 1518280447 192.168.0.196 9 typ host tcptype active generation 0\r
a=ice-ufrag:Oyef\r
a=ice-pwd:7d3SaR8dzTyBIvuaL0+jHlKr\r
a=ice-options:trickle\r
a=fingerprint:sha-256 19:E2:1C:3B:4B:9F:81:E6:B8:5C:F4:A5:A8:D8:73:04:BB:05:2F:70:9F:04:A9:0E:05:E9:26:33:E8:70:88:A2\r
a=setup:actpass\r
a=mid:0\r
a=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r
a=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r
a=extmap:3 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r
a=extmap:4 urn:ietf:params:rtp-hdrext:sdes:mid\r
a=sendrecv\r
a=msid:stream audiotrack\r
a=rtcp-mux\r
a=rtpmap:111 opus/48000/2\r
a=rtcp-fb:111 transport-cc\r
a=fmtp:111 minptime=10;useinbandfec=1\r
a=rtpmap:63 red/48000/2\r
a=fmtp:63 111/111\r
a=rtpmap:9 G722/8000\r
a=rtpmap:0 PCMU/8000\r
a=rtpmap:8 PCMA/8000\r
a=rtpmap:13 CN/8000\r
a=rtpmap:110 telephone-event/48000\r
a=rtpmap:126 telephone-event/8000\r
a=ssrc:1001 cname:Y7LCqvoNzR4m0f5E\r
a=ssrc:1001 msid:stream audiotrack\r
m=video 9 UDP/TLS/RTP/SAVPF 96 97 102 103 104 105 106 107 108 109 127 125 39 40 45 46 98 99\r
c=IN IP4 0.0.0.0\r
a=rtcp:9 IN IP4 0.0.0.0\r
a=ice-ufrag:Oyef\r
a=ice-pwd:7d3SaR8dzTyBIvuaL0+jHlKr\r
a=ice-options:trickle\r
a=fingerprint:sha-256 19:E2:1C:3B:4B:9F:81:E6:B8:5C:F4:A5:A8:D8:73:04:BB:05:2F:70:9F:04:A9:0E:05:E9:26:33:E8:70:88:A2\r
a=setup:actpass\r
a=mid:1\r
a=extmap:14 urn:ietf:params:rtp-hdrext:toffset\r
a=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r
a=extmap:13 urn:3gpp:video-orientation\r
a=extmap:3 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r
a=extmap:4 urn:ietf:params:rtp-hdrext:sdes:mid\r
a=extmap:10 urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id\r
a=extmap:11 urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id\r
a=sendonly\r
a=msid:stream videotrack\r
a=rtcp-mux\r
a=rtcp-rsize\r
a=rtpmap:96 VP8/90000\r
a=rtcp-fb:96 goog-remb\r
a=rtcp-fb:96 transport-cc\r
a=rtcp-fb:96 ccm fir\r
a=rtcp-fb:96 nack\r
a=rtcp-fb:96 nack pli\r
a=rtpmap:97 rtx/90000\r
a=fmtp:97 apt=96\r
a=rtpmap:102 H264/90000\r
a=rtcp-fb:102 goog-remb\r
a=rtcp-fb:102 transport-cc\r
a=rtcp-fb:102 ccm fir\r
a=rtcp-fb:102 nack\r
a=rtcp-fb:102 nack pli\r
a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f\r
a=rtpmap:103 rtx/90000\r
a=fmtp:103 apt=102\r
a=rtpmap:104 H264/90000\r
a=rtcp-fb:104 nack pli\r
a=fmtp:104 level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f\r
a=rtpmap:105 rtx/90000\r
a=fmtp:105 apt=104\r
a=rtpmap:106 H264/90000\r
a=rtcp-fb:106 nack pli\r
a=fmtp:106 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r
a=rtpmap:107 rtx/90000\r
a=fmtp:107 apt=106\r
a=rtpmap:108 H264/90000\r
a=rtcp-fb:108 nack pli\r
a=fmtp:108 level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42e01f\r
a=rtpmap:109 rtx/90000\r
a=fmtp:109 apt=108\r
a=rtpmap:127 H264/90000\r
a=rtcp-fb:127 nack pli\r
a=fmtp:127 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=4d001f\r
a=rtpmap:125 rtx/90000\r
a=fmtp:125 apt=127\r
a=rtpmap:39 H264/90000\r
a=rtcp-fb:39 nack pli\r
a=fmtp:39 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640034\r
a=rtpmap:40 rtx/90000\r
a=fmtp:40 apt=39\r
a=rtpmap:45 AV1/90000\r
a=rtcp-fb:45 nack pli\r
a=fmtp:45 level-idx=5;profile=0;tier=0\r
a=rtpmap:46 rtx/90000\r
a=fmtp:46 apt=45\r
a=rtpmap:98 VP9/90000\r
a=rtcp-fb:98 nack pli\r
a=fmtp:98 profile-id=0\r
a=rtpmap:99 rtx/90000\r
a=fmtp:99 apt=98\r
a=rid:h send\r
a=rid:m send\r
a=rid:l send\r
a=simulcast:send h;m;l\r
m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r
c=IN IP4 0.0.0.0\r
a=ice-ufrag:Oyef\r
a=ice-pwd:7d3SaR8dzTyBIvuaL0+jHlKr\r
a=ice-options:trickle\r
a=fingerprint:sha-256 19:E2:1C:3B:4B:9F:81:E6:B8:5C:F4:A5:A8:D8:73:04:BB:05:2F:70:9F:04:A9:0E:05:E9:26:33:E8:70:88:A2\r
a=setup:actpass\r
a=mid:2\r
a=sctp-port:5000\r
a=max-message-size:262144\r
";

/// FFmpeg の `-sdp_file` が書く SDP（LF 区切り、fmtp の `; ` 区切り）。
pub const FFMPEG_SDP: &str = "v=0
o=- 0 0 IN IP4 127.0.0.1
s=No Name
c=IN IP4 127.0.0.1
t=0 0
a=tool:libavformat 61.1.100
m=video 45000 RTP/AVP 96
b=AS:200
a=rtpmap:96 H264/90000
a=fmtp:96 packetization-mode=1; sprop-parameter-sets=Z2QAHqzZQKAv+XARAAADAAEAAAMAMg8WLZY=,aOvjyyLA; profile-level-id=64001E
m=audio 45002 RTP/AVP 97
b=AS:128
a=rtpmap:97 opus/48000/2
a=fmtp:97 sprop-stereo=1
";

/// mediamtx の DESCRIBE 応答に近い SDP。
pub const MEDIAMTX_SDP: &str = "v=0\r
o=- 0 0 IN IP4 127.0.0.1\r
s=Stream\r
c=IN IP4 0.0.0.0\r
t=0 0\r
m=video 0 RTP/AVP 96\r
a=control:rtsp://127.0.0.1:45554/cam/trackID=0\r
a=rtpmap:96 H264/90000\r
a=fmtp:96 packetization-mode=1; profile-level-id=64001E; sprop-parameter-sets=Z2QAHqzZQKAv+XARAAADAAEAAAMAMg8WLZY=,aOvjyyLA\r
m=audio 0 RTP/AVP 97\r
a=control:rtsp://127.0.0.1:45554/cam/trackID=1\r
a=rtpmap:97 mpeg4-generic/48000/2\r
a=fmtp:97 config=1190; indexdeltalength=3; indexlength=3; mode=AAC-hbr; profile-level-id=1; sizelength=13; streamtype=5\r
";

/// t= も o= もない古いカメラの SDP。
pub const OLD_CAMERA_SDP: &str = "v=0\r
s=\r
a=control:*\r
a=range:npt=0-\r
m=video 0 RTP/AVP 96\r
a=rtpmap:96 H265/90000\r
a=control:track1\r
";
