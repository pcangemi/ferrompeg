# Terminal demo shown in the README.
# Regenerate with: cargo build --release && vhs docs/demo.tape

Output docs/demo.gif

Set Shell "bash"
Set FontSize 15
Set Width 1200
Set Height 600
Set Padding 24
Set Theme "Catppuccin Mocha"
Set TypingSpeed 35ms

Hide
Type "export PATH=$PWD/target/release:$PATH PS1='$ ' && cd $(mktemp -d) && clear"
Enter
Show

Type "# Generate a tone and encode it to FLAC"
Enter
Type 'ferrompeg -hide_banner -f lavfi -i "sine=frequency=440:duration=5" tone.flac'
Enter
Sleep 2.5s

Type "ferroprobe -hide_banner tone.flac"
Enter
Sleep 2s

Type "# Audio filters and resampling"
Enter
Type 'ferrompeg -hide_banner -v error -i tone.flac -af "volume=-6dB,afade=t=out:st=4:d=1" -ar 16000 voice.wav'
Enter
Sleep 1s

Type "# Video: a Mandelbrot zoom, scaled and encoded to GIF"
Enter
Type 'ferrompeg -hide_banner -v error -f lavfi -i "mandelbrot=duration=3:rate=12" -vf "scale=240:-2" zoom.gif'
Enter
Sleep 2s
Type "du -h *"
Enter
Sleep 3s
