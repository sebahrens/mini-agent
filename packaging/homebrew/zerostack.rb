class Zerostack < Formula
  desc "Minimalistic coding agent written in Rust, optimized for memory footprint and performance"
  homepage "https://github.com/sebahrens/mini-agent"
  version "1.7.2"
  license "GPL-3.0-only"

  on_macos do
    if Hardware::CPU.intel?
      url "https://github.com/sebahrens/mini-agent/releases/download/v1.7.2/mini-agent-x86_64-apple-darwin.tar.gz"
      sha256 "20073c9b95629d8ca07716def668c7d8f52616571bbc435bb7a9c4db99719bfb"
    else
      url "https://github.com/sebahrens/mini-agent/releases/download/v1.7.2/mini-agent-aarch64-apple-darwin.tar.gz"
      sha256 "0ba49ebef99d0e9c113739ca0c32b4c5dcde85f772cfdfdccd9f75e58d082964"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/sebahrens/mini-agent/releases/download/v1.7.2/mini-agent-x86_64-unknown-linux-musl.tar.gz"
      sha256 "bee067458568c2b021dced37d91377301ee356d792b855c119f48f03cd698364"
    else
      url "https://github.com/sebahrens/mini-agent/releases/download/v1.7.2/mini-agent-aarch64-unknown-linux-musl.tar.gz"
      sha256 "0b1e28ed7552c38ebbea1f1c918ec1c3e5d4bab299c010b416aef1e2f74a9213"
    end
  end

  def install
    bin.install "mini-agent"
  end

  test do
    assert_match(/^mini-agent /, shell_output("#{bin}/mini-agent --version"))
  end
end
