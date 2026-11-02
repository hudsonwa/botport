# Draft formula for the future tap repo hudsonwa/homebrew-tap (copy to Formula/botport.rb there).
# Fill each sha256 from the release's SHA256SUMS; see packaging/homebrew/README.md.
class Botport < Formula
  desc "Port, sync and bridge AI bots between Grok Bot and Hermes"
  homepage "https://github.com/hudsonwa/botport"
  version "0.1.2"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/hudsonwa/botport/releases/download/v#{version}/botport-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_SHA256_OF_botport-aarch64-apple-darwin.tar.gz"
    end
    on_intel do
      url "https://github.com/hudsonwa/botport/releases/download/v#{version}/botport-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_SHA256_OF_botport-x86_64-apple-darwin.tar.gz"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/hudsonwa/botport/releases/download/v#{version}/botport-aarch64-unknown-linux-musl.tar.gz"
      sha256 "REPLACE_WITH_SHA256_OF_botport-aarch64-unknown-linux-musl.tar.gz"
    end
    on_intel do
      url "https://github.com/hudsonwa/botport/releases/download/v#{version}/botport-x86_64-unknown-linux-musl.tar.gz"
      sha256 "REPLACE_WITH_SHA256_OF_botport-x86_64-unknown-linux-musl.tar.gz"
    end
  end

  def install
    bin.install "botport", "grok2hermes", "g2h", "g2hd"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/botport --version")
  end
end
