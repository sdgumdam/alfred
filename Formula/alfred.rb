class Alfred < Formula
  desc "Workboss governance architecture working skeleton"
  homepage "https://github.com/sdgumdam/alfred"
  url "https://github.com/sdgumdam/alfred/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "09cbe872bc59e0b532f70986bd9a8c59375c5a214f3296c887b2fac9d4adcf69"
  license "Apache-2.0"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args(path: "crates/alfred-cli")
  end

  test do
    assert_match "alfred", shell_output("#{bin}/alfred --help")
    assert_match "plan", shell_output("#{bin}/alfred --help")
  end
end
