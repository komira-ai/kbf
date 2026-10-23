// Spike app: writes who it runs as to argv[1] (inside the lease) and tries a write outside
// the lease to argv[2] (the profile's write deny would refuse it only if the app inherited it).
import Foundation
import AppKit
let args = CommandLine.arguments
var out = "uid=\(getuid()) euid=\(geteuid()) pid=\(getpid()) ppid=\(getppid()) home=\(NSHomeDirectory())\n"
if args.count > 2 {
    let ok = FileManager.default.createFile(atPath: args[2], contents: Data("escaped".utf8))
    out += "escape_write=\(ok ? "SUCCEEDED" : "refused") path=\(args[2])\n"
}
if args.count > 1 { try? out.write(toFile: args[1], atomically: false, encoding: .utf8) }
FileHandle.standardError.write(Data(out.utf8))
let app = NSApplication.shared
out += "screens=\(NSScreen.screens.count) activation=\(app.setActivationPolicy(.regular))\n"
if args.count > 1 { try? out.write(toFile: args[1], atomically: false, encoding: .utf8) }
