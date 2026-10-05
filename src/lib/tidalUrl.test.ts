import { describe, it, expect } from "vitest";
import { parseTidalUrl } from "./tidalUrl";

const PLAYLIST = "0b5df1f8-4b1d-4c3e-9b0a-2f3c1d2e4a5b";

describe("parseTidalUrl", () => {
  it("plays a track", () => {
    expect(parseTidalUrl("tidal://track/123")).toEqual({
      kind: "playTrack",
      trackId: 123,
    });
  });

  it("navigates for plain album, artist, playlist and mix links", () => {
    expect(parseTidalUrl("tidal://album/42")).toEqual({
      kind: "navigate",
      view: { type: "album", albumId: 42 },
    });
    expect(parseTidalUrl("tidal://artist/7")).toEqual({
      kind: "navigate",
      view: { type: "artist", artistId: 7 },
    });
    expect(parseTidalUrl(`tidal://playlist/${PLAYLIST}`)).toEqual({
      kind: "navigate",
      view: { type: "playlist", playlistId: PLAYLIST },
    });
    expect(parseTidalUrl("tidal://mix/0123abc")).toEqual({
      kind: "navigate",
      view: { type: "mix", mixId: "0123abc" },
    });
  });

  it("plays a playlist, album or mix with a trailing /play", () => {
    expect(parseTidalUrl(`tidal://playlist/${PLAYLIST}/play`)).toEqual({
      kind: "playSource",
      sourceType: "playlist",
      id: PLAYLIST,
    });
    expect(parseTidalUrl("tidal://album/42/play")).toEqual({
      kind: "playSource",
      sourceType: "album",
      id: "42",
    });
    expect(parseTidalUrl("tidal://mix/0123abc/play")).toEqual({
      kind: "playSource",
      sourceType: "mix",
      id: "0123abc",
    });
  });

  it("accepts a trailing slash and any case on /play", () => {
    expect(parseTidalUrl(`tidal://playlist/${PLAYLIST}/PLAY/`)).toEqual({
      kind: "playSource",
      sourceType: "playlist",
      id: PLAYLIST,
    });
  });

  it("keeps navigating for an unknown suffix", () => {
    expect(parseTidalUrl(`tidal://playlist/${PLAYLIST}/edit`)).toEqual({
      kind: "navigate",
      view: { type: "playlist", playlistId: PLAYLIST },
    });
  });

  it("keeps navigating for artists, which have no play form", () => {
    expect(parseTidalUrl("tidal://artist/7/play")).toEqual({
      kind: "navigate",
      view: { type: "artist", artistId: 7 },
    });
  });

  it("rejects a play link with a non-numeric album id", () => {
    expect(parseTidalUrl("tidal://album/abc/play")).toBeNull();
  });

  it("rejects non-tidal and incomplete URLs", () => {
    expect(parseTidalUrl("https://tidal.com/playlist/x")).toBeNull();
    expect(parseTidalUrl("tidal://playlist")).toBeNull();
    expect(parseTidalUrl("tidal://video/1/play")).toBeNull();
  });
});
