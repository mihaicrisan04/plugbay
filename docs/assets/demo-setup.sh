#!/usr/bin/env bash
# Builds the throwaway world that demo.tape records in. Source it, don't run it:
#
#   source docs/assets/demo-setup.sh
#
# Everything lives under /private/tmp/plugbay-demo: a fake HOME (so paths print as
# `~/work/...` and macOS uses a throwaway keychain instead of your login keychain),
# PLUGBAY_HOME for plugbay's own config and state, and fake tokens for made-up accounts.
# Your real ~/.config/plugbay, keychain and agents are never touched.

demo_root=/private/tmp/plugbay-demo
plugbay_bin=${PLUGBAY_BIN:-$(pwd)/target/release/plugbay}

rm -rf "$demo_root"
mkdir -p "$demo_root/bin" "$demo_root/home/work/acme/api" "$demo_root/home/work/globex/web"
ln -s "$plugbay_bin" "$demo_root/bin/plugbay"

export HOME=$demo_root/home
export PLUGBAY_HOME=$demo_root/plugbay
export PATH=$demo_root/bin:/usr/bin:/bin
export PLUGBAY_LOG=error

# A keychain that only exists under the fake HOME, made the default there.
mkdir -p "$HOME/Library/Keychains" "$HOME/Library/Preferences"
demo_keychain=$HOME/Library/Keychains/demo.keychain-db
security create-keychain -p demo "$demo_keychain"
security set-keychain-settings "$demo_keychain"
security list-keychains -d user -s "$demo_keychain"
security default-keychain -d user -s "$demo_keychain"

cat > "$HOME/work/plugbay.toml" <<'TOML'
[servers]
linear = "agency"
TOML

cat > "$HOME/work/acme/plugbay.toml" <<'TOML'
account = "acme"

[servers]
sentry = true
TOML

cat > "$HOME/work/globex/plugbay.toml" <<'TOML'
account = "globex"

[servers]
sentry = true
linear = false
TOML

# Pretend every account is already logged in: fake tokens, stored the way
# `plugbay login --token` stores real ones, into the throwaway keychain.
for login in linear:agency sentry:acme sentry:globex posthog:acme; do
  account=${login#*:}
  dir=$HOME/work/$account
  [ "$account" = agency ] && dir=$HOME/work
  echo demo-token | plugbay -C "$dir" login "$login" --token >/dev/null
done
rm -rf "$PLUGBAY_HOME/state"

cd "$HOME/work" || return
export PS1='\n\[\e[38;5;245m\]\w\[\e[0m\] \[\e[38;5;141m\]❯\[\e[0m\] '
clear
