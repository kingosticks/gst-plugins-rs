# gst-plugins-spotify

This is a [GStreamer](https://gstreamer.freedesktop.org/) plugin to read content from
[Spotify](https://www.spotify.com/).

Make sure that your application follows [Spotify's design guidelines](https://developer.spotify.com/documentation/general/design-and-branding/)
to respect their legal/licensing restrictions.

## Spotify Credentials

This plugin requires a [Spotify Premium](https://www.spotify.com/premium/) account.

Provide a Spotify access token with 'streaming' scope using the `access-token` property. Such a token can be obtained by completing
[Spotify's OAuth flow](https://developer.spotify.com/documentation/web-api/concepts/authorization) or using the facility on their
[Web SDK getting started guide](https://developer.spotify.com/documentation/web-playback-sdk/tutorials/getting-started).
A token can also be obtained using [librespot-oauth](https://github.com/librespot-org/librespot/blob/dev/oauth/examples/oauth.rs):

```console
cargo install librespot-oauth --git https://github.com/librespot-org/librespot --example oauth && oauth
```

You may also want to cache credentials and downloaded files, see the `cache-` properties on the element.

## spotifyaudiosrc

The `spotifyaudiosrc` element can be used to play a song from Spotify using its [Spotify URI](https://community.spotify.com/t5/FAQs/What-s-a-Spotify-URI/ta-p/919201).

```
gst-launch-1.0 spotifyaudiosrc access-token=$ACCESS_TOKEN track=spotify:track:3i3P1mGpV9eRlfKccjDjwi ! oggdemux ! vorbisdec ! audioconvert ! autoaudiosink
```

The element also implements an URI handler which accepts credentials and cache settings as URI parameters:

```console
gst-launch-1.0 playbin3 uri=spotify:track:3i3P1mGpV9eRlfKccjDjwi?access-token=$ACCESS_TOKEN\&cache-credentials=cache\&cache-files=cache
```