The goal is to deliver a standard DASH origin file set to a CDN and then a browser via QUIC / WebTransport. 

The browser will buffer the DASH data, start dash.js, and intercept server requests via XHR.

STATUS: Properly formatted Vivoh WebTransport Media Packets are being created by the publisher and sent via the server to the client but the client is not yet parsing and rendering these correctly.

Server Command:
```
./vqd-server --cert cert.pem --key key.pem
```

Publisher Command:
```
./vqd-publisher --input /path/to/dash --server https://your.vqd-server.com
```

Encoder Command:
```
ffmpeg -re -stream_loop -1 -i adena.mp4 -vf "drawtext=text='Virginia\\: %{gmtime\\:%H\\\\\\:%M\\\\\\:%S.%3N}':fontsize=48:fontcolor=white:x=24:y=24" -c:v libx264 -preset ultrafast -tune zerolatency -g 30 -keyint_min 30 -sc_threshold 0 -b:v 3000k -c:a aac -b:a 128k -f dash -seg_duration 1 -use_timeline 1 -use_template 1 -init_seg_name 'init-$RepresentationID$.mp4' -media_seg_name 'chunk-$RepresentationID$-$Number%05d$.m4s' -window_size 5 -extra_window_size 5 -remove_at_exit 1 ./out/stream.mpd
```

Experimental Pipe Input Option with GPAC:

Publisher Command (no Encoder needed):

```
gpac -i adena.mp4:loop -f reframer:fps=30 ffenc:c=libx264:preset=fast:b=2000k:g=30:r=30 ffenc:c=aac:b=128k -o stdout:ext=mp4:frag:cdur=1:cmaf=cmfc:box=tfdt:mvex:split_mode=tracks:subs_sidx=0:\!tsalign | /home/ubuntu/vivoh-quic-dash/target/release/vqd-publisher --pipe --server https://va01.wtmpeg.com/live/pub
```
