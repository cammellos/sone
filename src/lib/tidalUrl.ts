import type { AppView } from "../types";

type PlayableSourceType = "playlist" | "album" | "mix";

export type DeepLinkAction =
  | { kind: "navigate"; view: AppView }
  | { kind: "playTrack"; trackId: number }
  | { kind: "playSource"; sourceType: PlayableSourceType; id: string };

export function parseTidalUrl(url: string): DeepLinkAction | null {
  if (!url.startsWith("tidal://")) return null;

  const path = url.slice(8); // strip "tidal://"
  const parts = path.split("/").filter(Boolean);
  if (parts.length < 2) return null;

  const contentType = parts[0].toLowerCase();
  const contentId = parts[1];
  // tidal://<playlist|album|mix>/<id>/play starts playback instead of navigating
  const play = parts[2]?.toLowerCase() === "play";

  switch (contentType) {
    case "track": {
      const id = parseInt(contentId, 10);
      if (isNaN(id)) return null;
      return { kind: "playTrack", trackId: id };
    }
    case "album": {
      const id = parseInt(contentId, 10);
      if (isNaN(id)) return null;
      if (play)
        return { kind: "playSource", sourceType: "album", id: String(id) };
      return { kind: "navigate", view: { type: "album", albumId: id } };
    }
    case "artist": {
      const id = parseInt(contentId, 10);
      if (isNaN(id)) return null;
      return { kind: "navigate", view: { type: "artist", artistId: id } };
    }
    case "playlist":
      if (play)
        return { kind: "playSource", sourceType: "playlist", id: contentId };
      return {
        kind: "navigate",
        view: { type: "playlist", playlistId: contentId },
      };
    case "mix":
      if (play) return { kind: "playSource", sourceType: "mix", id: contentId };
      return { kind: "navigate", view: { type: "mix", mixId: contentId } };
    default:
      return null;
  }
}
