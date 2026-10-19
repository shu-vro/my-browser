// my-browser-translate: Translate Page for my-browser, on this Mac.
//
// The browser starts this helper on first use and talks to it over pipes:
// one JSON request per line on stdin ({"id", "to", "texts"}), one JSON reply
// per line on stdout ({"id", "from", "texts"} or {"id", "error", "message"}).
// Apple's Translation framework and NaturalLanguage do the work on-device:
// nothing is sent anywhere. The browser closes stdin after a minute without
// requests, and the helper exits, so it costs nothing when unused.
//
// MYB_TRANSLATE_FAKE=1 (tests): "translates" by upper-casing, so the
// browser's side can be tested without language models installed.

import Foundation
import NaturalLanguage
import Translation

struct Request: Decodable {
    let id: Int
    let to: String?
    let texts: [String]
}

func reply(_ object: [String: Any]) {
    guard let data = try? JSONSerialization.data(withJSONObject: object) else { return }
    FileHandle.standardOutput.write(data + Data([0x0A]))
}

func name(_ code: String) -> String {
    Locale.current.localizedString(forLanguageCode: code) ?? code
}

func fail(_ id: Int, _ error: String, _ message: String) {
    reply(["id": id, "error": error, "message": message])
}

func handle(_ r: Request, fake: Bool) async {
    let recognizer = NLLanguageRecognizer()
    recognizer.processString(String(r.texts.joined(separator: "\n").prefix(5000)))
    guard let detected = recognizer.dominantLanguage, detected != .undetermined else {
        return fail(r.id, "unknown", "Couldn't tell which language this page is in.")
    }
    let to = (r.to ?? "").isEmpty ? (Locale.preferredLanguages.first ?? "en") : r.to!
    let source = Locale.Language(identifier: detected.rawValue)
    let target = Locale.Language(identifier: to)
    let from = source.languageCode?.identifier ?? detected.rawValue
    let into = target.languageCode?.identifier ?? to
    if from == into {
        return fail(r.id, "same", "This page is already in \(name(into)).")
    }
    if fake {
        return reply(["id": r.id, "from": from, "texts": r.texts.map { $0.uppercased() }])
    }
    guard #available(macOS 26.0, *) else {
        return fail(r.id, "old", "Translating pages needs macOS 26 or later.")
    }
    switch await LanguageAvailability().status(from: source, to: target) {
    case .installed:
        break
    case .supported:
        return fail(
            r.id, "install",
            "To translate \(name(from)) to \(name(into)), download both languages in System Settings → General → Language & Region → Translation Languages. Translation then runs on this Mac.")
    default:
        return fail(r.id, "unsupported", "macOS can't translate \(name(from)) to \(name(into)).")
    }
    do {
        let session = TranslationSession(installedSource: source, target: target)
        let requests = r.texts.enumerated().map {
            TranslationSession.Request(sourceText: $0.element, clientIdentifier: String($0.offset))
        }
        var texts = r.texts
        for response in try await session.translations(from: requests) {
            if let i = Int(response.clientIdentifier ?? ""), i < texts.count {
                texts[i] = response.targetText
            }
        }
        reply(["id": r.id, "from": from, "texts": texts])
    } catch {
        fail(r.id, "failed", "The translation failed: \(error.localizedDescription)")
    }
}

@main
struct Main {
    static func main() async {
        let fake = ProcessInfo.processInfo.environment["MYB_TRANSLATE_FAKE"] == "1"
        while let line = readLine() {
            guard let request = try? JSONDecoder().decode(Request.self, from: Data(line.utf8)) else { continue }
            await handle(request, fake: fake)
        }
    }
}
