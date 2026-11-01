"""Native ABI and waited-process peak support for reproducible benchmarks."""
import subprocess


def stat_abi_bytes(root):
    source=root/'resource-observer.c';binary=root/'resource-observer'
    source.write_text(r'''#define _DEFAULT_SOURCE
#include <sys/stat.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <signal.h>
#include <unistd.h>
#include <errno.h>
#include <stdio.h>
#include <string.h>
int main(int argc,char **argv){
 if(argc==2 && !strcmp(argv[1],"--stat-size")){printf("%zu",sizeof(struct stat));return 0;}
 if(argc<3)return 125;
 /* A timeout terminates the measured process group. Keep this observer alive
    until its child finishes graceful cleanup of separately owned groups. */
 struct sigaction action={0};action.sa_handler=SIG_IGN;sigemptyset(&action.sa_mask);
 if(sigaction(SIGTERM,&action,NULL))return 125;
 pid_t pid=fork();if(pid<0)return 125;
 if(pid==0){
  action.sa_handler=SIG_DFL;if(sigaction(SIGTERM,&action,NULL))_exit(125);
  execvp(argv[2],argv+2);_exit(127);
 }
 struct rusage usage;int status;
 while(wait4(pid,&status,0,&usage)<0){if(errno!=EINTR)return 125;}
 FILE *out=fopen(argv[1],"w");if(!out)return 125;
 long resident=usage.ru_maxrss;
#ifdef __APPLE__
 resident=(resident+1023)/1024;
#endif
 fprintf(out,"%ld\n",resident);if(fclose(out))return 125;
 return WIFEXITED(status)?WEXITSTATUS(status):128+WTERMSIG(status);
}
''')
    subprocess.run(['cc',str(source),'-o',str(binary)],check=True,capture_output=True)
    return int(subprocess.check_output([str(binary),'--stat-size']))

