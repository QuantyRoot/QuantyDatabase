# The version and the three checksums are rewritten by the release
# workflow when a tag is built, from the SHA256SUMS it just produced.
# Editing them by hand means editing them wrong.
class Quantydb < Formula
  desc "One database that reshapes itself into whatever you need"
  homepage "https://github.com/QuantyRoot/QuantyDatabase"
  version "0.0.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/QuantyRoot/QuantyDatabase/releases/download/v#{version}/quantydb-macos-arm64"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
    on_intel do
      url "https://github.com/QuantyRoot/QuantyDatabase/releases/download/v#{version}/quantydb-macos-x86_64"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/QuantyRoot/QuantyDatabase/releases/download/v#{version}/quantydb-linux-x86_64"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
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
