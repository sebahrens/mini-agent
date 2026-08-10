class Zerostack < Formula
  desc "Minimalistic coding agent written in Rust, optimized for memory footprint and performance"
  homepage "https://github.com/sebahrens/mini-agent"
  version "1.7.2"
  revision 1
  license "GPL-3.0-only"

  on_macos do
    if Hardware::CPU.intel?
      url "https://github.com/sebahrens/mini-agent/releases/download/v1.7.2/mini-agent-x86_64-apple-darwin.tar.gz"
      sha256 "9d3799415d8598b45e21c7a9b5a3ddd39312c8082b817f1d75f55753bf87ae5c"
    else
      url "https://github.com/sebahrens/mini-agent/releases/download/v1.7.2/mini-agent-aarch64-apple-darwin.tar.gz"
      sha256 "c5df64defcb761296f28ba84d2011492499819dac09777f8d6ecc857f6483755"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/sebahrens/mini-agent/releases/download/v1.7.2/mini-agent-x86_64-unknown-linux-musl.tar.gz"
      sha256 "d0bae6b5b7813f4a4fe1aebf1ee5aeaac97e64698781a16cd00728c3d14f3f97"
    else
      url "https://github.com/sebahrens/mini-agent/releases/download/v1.7.2/mini-agent-aarch64-unknown-linux-musl.tar.gz"
      sha256 "eb62b7cbe74f21ed0aea74dc9ce79a0675a0bfbdbb42799e0e6c7122c5866604"
    end
  end

  def install
    bin.install "mini-agent"
    pkgshare.install "LICENSE", "NOTICE", "SOURCE.md"
  end

  test do
    assert_match(/^mini-agent /, shell_output("#{bin}/mini-agent --version"))
  end
end
