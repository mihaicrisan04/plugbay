# Template for Formula/plugbay.rb in mihaicrisan04/homebrew-tap.
# The release workflow fills in the @...@ placeholders with packaging/homebrew/render.sh;
# this file is not installable as is. See docs/releasing.md.
class Plugbay < Formula
  desc "Per-directory MCP servers and accounts for every AI agent"
  homepage "https://github.com/mihaicrisan04/plugbay"
  license "Apache-2.0"

  livecheck do
    url :stable
    strategy :github_latest
  end

  on_macos do
    on_arm do
      url "https://github.com/mihaicrisan04/plugbay/releases/download/v@VERSION@/plugbay-v@VERSION@-aarch64-apple-darwin.tar.gz"
      sha256 "@SHA256_AARCH64_APPLE_DARWIN@"
    end
    on_intel do
      url "https://github.com/mihaicrisan04/plugbay/releases/download/v@VERSION@/plugbay-v@VERSION@-x86_64-apple-darwin.tar.gz"
      sha256 "@SHA256_X86_64_APPLE_DARWIN@"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/mihaicrisan04/plugbay/releases/download/v@VERSION@/plugbay-v@VERSION@-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "@SHA256_AARCH64_UNKNOWN_LINUX_GNU@"
    end
    on_intel do
      url "https://github.com/mihaicrisan04/plugbay/releases/download/v@VERSION@/plugbay-v@VERSION@-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "@SHA256_X86_64_UNKNOWN_LINUX_GNU@"
    end
  end

  def install
    bin.install "plugbay"
    generate_completions_from_executable(bin/"plugbay", "completion")
    doc.install "README.md", "LICENSE"
  end

  def caveats
    s = <<~EOS
      Move the MCP servers you already have in Claude Code and Codex into plugbay:
        plugbay migrate            # shows the plan; add --apply to do it, --undo to roll back

      Starting fresh instead? Register plugbay once, then pin servers per directory:
        plugbay setup
        plugbay use linear:work    # writes ./plugbay.toml
      The daemon starts on demand when an agent connects and exits when idle,
      so there is no service to manage. Check the installation with:
        plugbay doctor
    EOS
    on_linux do
      s += <<~EOS

        Logins and tokens are stored through the Secret Service API, so a keyring
        daemon (GNOME Keyring or KWallet) must be running in your session.
      EOS
    end
    s
  end

  test do
    assert_match(/^plugbay \d+\.\d+\.\d+/, shell_output("#{bin}/plugbay --version"))
  end
end
