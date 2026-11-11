#
# spec file for package wallspin
#
Name:           wallspin
Version:        0.1.1
Release:        0
Summary:        Wallpaper changer for GNOME with topics, per-monitor images and a clock
License:        MIT
URL:            https://github.com/darkobas2/wallspin
Source0:        %{url}/archive/v%{version}/%{name}-%{version}.tar.gz
Source1:        vendor.tar.zst
BuildRequires:  cargo-packaging
BuildRequires:  pkgconfig(gtk4)
BuildRequires:  pkgconfig(libadwaita-1)
BuildRequires:  systemd-rpm-macros
Requires:       fontconfig
Requires:       glib2-tools
Recommends:     zenity

%description
Wallspin changes your GNOME wallpaper from topics you pick (Wallhaven, ArtStation,
Bing, NASA APOD, Unsplash, local folders), can show a different image on each
monitor, draws a clock onto the wallpaper and can use a separate lock-screen image.
It renders into a single file rewritten in place, which avoids GNOME Shell's
unbounded background texture cache.

%prep
%autosetup -a1 -p1

%build
%{cargo_build}

%install
install -Dm755 target/release/%{name} %{buildroot}%{_bindir}/%{name}
install -Dm644 data/io.github.Wallspin.desktop %{buildroot}%{_datadir}/applications/io.github.Wallspin.desktop
install -Dm644 data/icons/wallspin.svg %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/io.github.Wallspin.svg
install -Dm644 data/icons/wallspin-symbolic.svg %{buildroot}%{_datadir}/icons/hicolor/symbolic/apps/io.github.Wallspin-symbolic.svg
sed 's|ExecStart=.*|ExecStart=%{_bindir}/%{name} daemon|' contrib/wallspin.service > wallspin.service
install -Dm644 wallspin.service %{buildroot}%{_userunitdir}/wallspin.service

%check
%{cargo_test}

%files
%license LICENSE
%doc README.md
%{_bindir}/%{name}
%{_datadir}/applications/io.github.Wallspin.desktop
%dir %{_datadir}/icons/hicolor
%dir %{_datadir}/icons/hicolor/scalable
%dir %{_datadir}/icons/hicolor/scalable/apps
%dir %{_datadir}/icons/hicolor/symbolic
%dir %{_datadir}/icons/hicolor/symbolic/apps
%{_datadir}/icons/hicolor/scalable/apps/io.github.Wallspin.svg
%{_datadir}/icons/hicolor/symbolic/apps/io.github.Wallspin-symbolic.svg
%{_userunitdir}/wallspin.service

%changelog
