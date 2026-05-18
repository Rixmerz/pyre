Name:           pyre
Version:        0.1.0
Release:        1%{?dist}
Summary:        Daemon-owned terminal multiplexer with block model and agent observability
License:        MIT OR Apache-2.0
URL:            https://github.com/<TODO>/pyre
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  rust cargo

%description
pyre is a Linux-first terminal multiplexer built around a persistent daemon
(pyred) that owns PTYs and survives client disconnects. Every command is a
Block: persisted to SQLite, searchable via Tantivy, and exposed to AI agents
through an MCP server.

%prep
%autosetup

%build
cargo build --release --locked

%install
for bin in pyred pyrec pyre-tui pyre-mcp; do
    install -Dm755 target/release/$bin %{buildroot}%{_bindir}/$bin
done
install -Dm644 dist/systemd/pyred.service \
    %{buildroot}%{_userunitdir}/pyred.service
# Man pages (if pre-built with pandoc)
for page in docs/man/*.1; do
    [ -f "$page" ] && install -Dm644 "$page" \
        %{buildroot}%{_mandir}/man1/$(basename "$page")
done

%files
%license LICENSE-MIT
%{_bindir}/pyred
%{_bindir}/pyrec
%{_bindir}/pyre-tui
%{_bindir}/pyre-mcp
%{_userunitdir}/pyred.service
%optional %{_mandir}/man1/pyred.1*
%optional %{_mandir}/man1/pyrec.1*
%optional %{_mandir}/man1/pyre-tui.1*
%optional %{_mandir}/man1/pyre-mcp.1*

%changelog
* Sun May 18 2026 pyre contributors <noreply@example.com> - 0.1.0-1
- Initial release
