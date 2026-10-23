#!/usr/bin/env python3
"""Build an unsigned native bundle. Never install, register, sign or activate it."""
import argparse
from pathlib import Path
import plistlib
import subprocess
import sys
import tomllib

p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--output',type=Path,default=Path('target/native'))
a=p.parse_args()
if sys.platform!='darwin':p.error('native macOS build required')
root=Path(__file__).resolve().parents[1]
version=tomllib.loads((root/'Cargo.toml').read_text())['package']['version']
out=a.output.resolve();out.mkdir(parents=True,exist_ok=True)
app=out/'GitOn.app';extension=app/'Contents/PlugIns/GitOnFS.appex'
for bundle in (app,extension):(bundle/'Contents/MacOS').mkdir(parents=True,exist_ok=True)
def plist(path,contents):
    path.write_bytes(plistlib.dumps(contents,sort_keys=True))
def metadata(identifier,executable,kind):
    return dict(CFBundleIdentifier=identifier,CFBundleExecutable=executable,CFBundleName=executable,CFBundlePackageType=kind,CFBundleShortVersionString=version,CFBundleVersion=version,CFBundleInfoDictionaryVersion='6.0',LSMinimumSystemVersion='26.4')
app_info=metadata('dev.progbeat.GitOn','GitOn','APPL')
app_info['LSBackgroundOnly']=True
plist(app/'Contents/Info.plist',app_info)
info=metadata('dev.progbeat.GitOn.FileSystem','GitOnFS','XPC!')
info['EXAppExtensionAttributes']=dict(EXExtensionPointIdentifier='com.apple.fskit.fsmodule',FSShortName='giton',FSSupportsPathURLs=True,FSSupportsBlockResources=False,FSRequiresSecurityScopedPathURLResources=True,FSMediaTypes={},FSPersonalities={},FSActivateOptionSyntax={'shortOptions':'u:g:m:o:'})
plist(extension/'Contents/Info.plist',info)
plist(out/'GitOnFS.entitlements',{'com.apple.security.app-sandbox':True,'com.apple.developer.fskit.fsmodule':True,'com.apple.security.network.client':True})
cache=out/'module-cache'
subprocess.run(['xcrun','clang','-fobjc-arc','-fblocks','-fmodules','-fmodules-cache-path='+str(cache),'-mmacosx-version-min=26.4','-Wall','-Wextra','-Werror','-Wno-unused-parameter','-c',str(root/'macos/GitOnFS/GitOnFS.m'),'-o',str(out/'adapter.o')],check=True)
subprocess.run(['xcrun','swiftc','-parse-as-library','-target','arm64-apple-macos26.4','-module-cache-path',str(cache),'-import-objc-header',str(root/'macos/GitOnFS/GitOnFS.h'),str(root/'macos/GitOnFS/Extension.swift'),str(out/'adapter.o'),'-framework','FSKit','-o',str(extension/'Contents/MacOS/GitOnFS')],check=True)
subprocess.run(['xcrun','swiftc','-target','arm64-apple-macos26.4','-module-cache-path',str(cache),str(root/'macos/Host.swift'),'-o',str(app/'Contents/MacOS/GitOn')],check=True)
print(app)
print('Unsigned build only; installation, registration, signing and activation need separate owner approval.')
