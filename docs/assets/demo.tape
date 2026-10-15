# The README demo. Record it from the repository root:
#
#   cargo build --release && vhs docs/assets/demo.tape
#
# demo-setup.sh builds a throwaway world under /private/tmp/plugbay-demo (fake HOME,
# PLUGBAY_HOME, keychain and tokens), so no real config, account or token appears.

Output docs/assets/demo.gif

Require bash
Set Shell bash
Set Theme "Catppuccin Mocha"
Set FontSize 16
Set Width 960
Set Height 640
Set Padding 24
Set WindowBar Colorful
Set BorderRadius 8
Set TypingSpeed 35ms
Set PlaybackSpeed 1

Hide
Type "export TERM=xterm-256color && source docs/assets/demo-setup.sh"
Enter
Sleep 2s
Show

# One file per level: the agency's Linear everywhere, one Sentry account per client.
Type "tail -n +1 plugbay.toml acme/plugbay.toml globex/plugbay.toml"
Sleep 300ms
Enter
Sleep 3.5s

Hide
Type "clear"
Enter
Show

# Same server, different account, depending on where the agent starts.
Type "cd acme/api && plugbay ls"
Sleep 300ms
Enter
Sleep 3s

Type "plugbay -C ~/work/globex/web ls"
Sleep 300ms
Enter
Sleep 3.5s

Hide
Type "clear"
Enter
Show

# Pin one more server here; it inherits the tree's account.
Type "plugbay use posthog && plugbay ls"
Sleep 300ms
Enter
Sleep 3.5s

Hide
Type "clear"
Enter
Show

Type "plugbay why sentry"
Sleep 300ms
Enter
Sleep 3.5s
