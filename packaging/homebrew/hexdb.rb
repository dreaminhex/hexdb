# Homebrew formula for a tap (dreaminhex/homebrew-hexdb):
#   brew tap dreaminhex/hexdb && brew install hexdb
#
# The release workflow publishes the archives and SHA256SUMS; update `version`
# and the two sha256 values from SHA256SUMS for each release.
class Hexdb < Formula
  desc "Hexagonal document database: server, CLI and admin UI"
  homepage "https://github.com/dreaminhex/hexdb"
  version "1.0.0"

  on_macos do
    on_arm do
      url "https://github.com/dreaminhex/hexdb/releases/download/v#{version}/hexdb-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_SHA256_FROM_SHA256SUMS"
    end
    on_intel do
      url "https://github.com/dreaminhex/hexdb/releases/download/v#{version}/hexdb-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_SHA256_FROM_SHA256SUMS"
    end
  end

  def install
    bin.install "hexdb_api", "hexdb"
    pkgshare.install "ui"
    (etc/"hexdb").mkpath
    inreplace "hexdb.toml" do |s|
      s.gsub! 'path = "./data"', "path = \"#{var}/hexdb\""
      s.gsub! 'path = "./ui"', "path = \"#{opt_pkgshare}/ui\""
    end
    etc.install "hexdb.toml" => "hexdb/hexdb.toml" unless (etc/"hexdb/hexdb.toml").exist?
    doc.install "README.md", "MANUAL.md"
  end

  def post_install
    (var/"hexdb").mkpath
    local = etc/"hexdb/hexdb.local.toml"
    unless local.exist?
      local.write "[storage]\nencryption_key = \"#{Utils.safe_popen_read(bin/"hexdb", "secret").strip}\"\n"
      local.chmod 0600
    end
  end

  service do
    run [opt_bin/"hexdb_api", "--config", etc/"hexdb/hexdb.toml"]
    keep_alive true
    log_path var/"log/hexdb.log"
    error_log_path var/"log/hexdb.log"
  end

  test do
    assert_match "hexdb_api", shell_output("#{bin}/hexdb_api --version")
  end
end
