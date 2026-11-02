import CSenca
import Foundation

/// VISCA over IP のコマンド（senca.h の SENCA_VISCA_* と同じ番号）。
public struct ViscaCommand: Equatable, Sendable {
    public var kind: Int32
    public var a: Int32 = 0, b: Int32 = 0, c: Int32 = 0, d: Int32 = 0

    public static let zoomStop: Int32 = 2, zoomTele: Int32 = 3, zoomWide: Int32 = 4, zoomDirect: Int32 = 5
    public static let focusAuto: Int32 = 10, focusOnePush: Int32 = 12
    public static let presetReset: Int32 = 15, presetSet: Int32 = 16, presetRecall: Int32 = 17
    public static let zoomPositionInquiry: Int32 = 20

    /// 受けたパケットを読む。知らないコマンドは kind = 0（seq は入る）、壊れていれば nil。
    public static func parse(_ packet: Data) -> (seq: UInt32, command: ViscaCommand)? {
        var seq: UInt32 = 0
        var c = senca_visca_command()
        let r = packet.withUnsafeBytes { p in
            senca_visca_parse(p.bindMemory(to: UInt8.self).baseAddress, p.count, &seq, &c)
        }
        if r == SENCA_OK { return (seq, ViscaCommand(kind: c.kind, a: c.a, b: c.b, c: c.c, d: c.d)) }
        if r == SENCA_ERR_UNSUPPORTED { return (seq, ViscaCommand(kind: 0)) }
        return nil
    }

    public enum Reply: Int32 { case ack = 1, completion = 2, error = 3 }

    /// 返事のパケット（error の code: 0x41 = 実行できない、0x02 = 構文の誤り）。
    public static func reply(_ kind: Reply, seq: UInt32, errorCode: UInt32 = 0, data: Data = Data()) -> Data {
        let cap = 32
        var buf = [UInt8](repeating: 0, count: cap)
        var n = 0
        let r = data.withUnsafeBytes { p in
            senca_visca_build_reply(kind.rawValue, 1, errorCode, p.bindMemory(to: UInt8.self).baseAddress, p.count, seq, &buf, cap, &n)
        }
        return r == SENCA_OK ? Data(buf[..<n]) : Data()
    }
}
