# The version and the three checksums are rewritten by the release
# workflow when a tag is built, from the SHA256SUMS it just produced.
# Editing them by hand means editing them wrong.
class Quantydb < Formula
  desc "One database that reshapes itself into whatever you need"
  homepage "https://github.com/QuantyRoot/QuantyDatabase"
  version "0.4.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/QuantyRoot/QuantyDatabase/releases/download/v#{version}/quantydb-macos-arm64"
      sha256 "cbdb1f29d30aa39df00f83f553b8ddb5c7387c7a55e67398424115e23d6c1d11"
    end
    on_intel do
      url "https://github.com/QuantyRoot/QuantyDatabase/releases/download/v#{version}/quantydb-macos-x86_64"
      sha256 "f6bc0263e6e80e33c25b2bb65697f062b2e548fdbdab942c05d26b2d247ea47e"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/QuantyRoot/QuantyDatabase/releases/download/v#{version}/quantydb-linux-x86_64"
      sha256 "e1ba7d283b8add76d1eb38bff47b1c3a48cbd42c3566884842496ddf8ed0c282"
    end
  end

  def install
    # The downloaded file is named for its platform; the installed one is
    # named for the program.
    bin.install Dir["quantydb-*"].first => "quantydb"
  end

  def caveats
    <<~TEXT
      The server speaks plaintext. TLS is written here rather than pulled
      in and is not written yet, so keep `quantydb serve` on loopback or a
      network you trust.

      `quantydb serve` runs on Linux. On macOS the tool, the library and
      both query languages work; the server does not.

      Start with `quantydb setup`.
    TEXT
  end

  test do
    assert_match "quantydb", shell_output("#{bin}/quantydb about")
    system bin/"quantydb", "create", testpath/"t.qdb"
    assert_predicate testpath/"t.qdb", :exist?
  end
end
