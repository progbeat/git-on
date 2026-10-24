#!/usr/bin/env python3
"""Exercise the unmodified cargo-dist installer against native local artifacts."""
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
from threading import Thread

directory=Path(sys.argv[1]).resolve()
metadata=json.loads(subprocess.check_output(['cargo','metadata','--no-deps','--format-version=1']))
version=metadata['packages'][0]['version']


class Handler(SimpleHTTPRequestHandler):
    def log_message(self,*args):pass


server=ThreadingHTTPServer(('127.0.0.1',0),partial(Handler,directory=str(directory)))
thread=Thread(target=server.serve_forever,daemon=True)
thread.start()
try:
    with tempfile.TemporaryDirectory() as temporary:
        home=Path(temporary)
        env={key:value for key,value in os.environ.items() if not key.startswith('GIT_ON_')}
        env.update(HOME=str(home),XDG_CONFIG_HOME=str(home/'config'),XDG_CACHE_HOME=str(home/'cache'),
                   TMPDIR=str(home),PATH='/usr/bin:/bin',NO_PROXY='127.0.0.1',
                   GIT_ON_INSTALL_DIR=str(home/'bin'),
                   GIT_ON_DOWNLOAD_URL=f'http://127.0.0.1:{server.server_port}')
        installer=['/bin/sh',str(directory/'git-on-installer.sh')]
        subprocess.run(installer+['--help'],env=env,check=True,capture_output=True,timeout=30)
        binary=home/'bin/git-on'
        assert not binary.exists(),'help installed a binary'
        for arguments in [[],['--quiet']]:
            result=subprocess.run(installer+arguments,env=env,capture_output=True,timeout=60)
            assert result.returncode==0,(result.stdout,result.stderr)
            assert subprocess.check_output([str(binary),'--version'])==f'git-on {version}\n'.encode()
        print('Stock installer help, install and reinstall passed:',version)
finally:
    server.shutdown()
    server.server_close()
    thread.join()
