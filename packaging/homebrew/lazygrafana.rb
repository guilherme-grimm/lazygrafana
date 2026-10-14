# Homebrew formula template for a tap (e.g. guilherme-grimm/homebrew-tap).
# Update version and sha256 values from the release's .sha256 files on each release.
class Lazygrafana < Formula
  desc "Lazygit-style terminal UI for operating Grafana"
  homepage "https://github.com/guilherme-grimm/lazygrafana"
  version "0.1.0"
  license any_of: ["MIT", "Apache-2.0"]

  on_macos do
    on_arm do
      url "https://github.com/guilherme-grimm/lazygrafana/releases/download/v#{version}/lazygrafana-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_SHA256"
    end
    on_intel do
      url "https://github.com/guilherme-grimm/lazygrafana/releases/download/v#{version}/lazygrafana-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_SHA256"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/guilherme-grimm/lazygrafana/releases/download/v#{version}/lazygrafana-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_SHA256"
    end
    on_intel do
      url "https://github.com/guilherme-grimm/lazygrafana/releases/download/v#{version}/lazygrafana-v#{version}-x86_64-unknown-linux-musl.tar.gz"
      sha256 "REPLACE_WITH_SHA256"
    end
  end

  def install
    bin.install "lazygrafana"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/lazygrafana --version")
  end
end
