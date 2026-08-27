import assert from "node:assert/strict";
import fs from "node:fs";
import test from "node:test";

const read = (path) => fs.readFileSync(path, "utf8");

test("release distribution is limited to Windows x64 and macOS", () => {
  const release = read(".github/workflows/release.yml");
  const manualWindows = read(".github/workflows/build-windows-msi.yml");
  const ci = read(".github/workflows/ci.yml");
  const upload = read(".github/workflows/upload-r2.yml");

  for (const target of [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
  ]) {
    assert.match(release, new RegExp(target.replaceAll("-", "\\-")));
    assert.match(upload, new RegExp(target.replaceAll("-", "\\-")));
  }

  for (const unsupported of [
    "i686-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "windows-x86",
    "win32",
  ]) {
    assert.doesNotMatch(release, new RegExp(unsupported.replaceAll("-", "\\-")));
    assert.doesNotMatch(upload, new RegExp(unsupported.replaceAll("-", "\\-")));
  }

  assert.doesNotMatch(manualWindows, /architecture:|i686|win32|inputs\.architecture/);
  assert.match(manualWindows, /WINDOWS_TARGET: x86_64-pc-windows-msvc/);
  assert.match(manualWindows, /WINDOWS_WIX_ARCH: x64/);

  assert.doesNotMatch(ci, /- linux|platform == 'linux'|i686-pc-windows-msvc/);
  assert.match(ci, /matrix="\[\$macos,\$windows\]"/);
});

test("obsolete Linux validation workflows are removed", () => {
  assert.equal(fs.existsSync(".github/workflows/validate-linux-x64-packages.yml"), false);
  assert.equal(fs.existsSync(".github/workflows/validate-linux-arm64-packages.yml"), false);
});

test("installation documentation advertises only Windows x64 and macOS", () => {
  for (const path of ["README.md", "README_CN.md"]) {
    const readme = read(path);
    const install = readme.split(/^## (?:Install|安装)$/m)[1].split(/^## /m)[0];
    assert.match(install, /macOS/);
    assert.match(install, /Windows/);
    assert.doesNotMatch(install, /Linux|win32|32-bit|32 位/);
  }
});

test("yellow application artwork is packaged for Windows and macOS", () => {
  const png = fs.readFileSync("resources/navop-icon.png");
  assert.equal(png.subarray(1, 4).toString("ascii"), "PNG");
  assert.equal(png.readUInt32BE(16), 1024);
  assert.equal(png.readUInt32BE(20), 1024);
  assert.equal(png[25], 6, "master PNG must retain RGBA transparency");

  const ico = fs.readFileSync("resources/windows/navop.ico");
  assert.equal(ico.readUInt16LE(0), 0);
  assert.equal(ico.readUInt16LE(2), 1);
  assert.equal(ico.readUInt16LE(4), 7);
  const icoSizes = Array.from({ length: 7 }, (_, index) => {
    const encoded = ico[6 + index * 16];
    return encoded === 0 ? 256 : encoded;
  });
  assert.deepEqual(icoSizes, [16, 24, 32, 48, 64, 128, 256]);

  const icns = fs.readFileSync("resources/macos/Navop.icns");
  assert.equal(icns.subarray(0, 4).toString("ascii"), "icns");
  assert.equal(icns.readUInt32BE(4), icns.length);
  const chunkTypes = [];
  for (let offset = 8; offset < icns.length; ) {
    chunkTypes.push(icns.subarray(offset, offset + 4).toString("ascii"));
    offset += icns.readUInt32BE(offset + 4);
  }
  assert.deepEqual(chunkTypes, ["icp4", "icp5", "icp6", "ic07", "ic08", "ic09", "ic10"]);
});
