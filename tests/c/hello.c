/* The smallest check of C on Veda: output, arguments, the heap, the
   process id and the working directory. Exits with 42 so that the caller
   can tell it ran to the end. */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv)
{
	printf("Hello from C on Veda! argc=%d argv[0]=%s\n", argc, argv[0]);
	char *p = malloc(100000);
	if (!p)
		return 1;
	memset(p, 1, 100000);
	printf("pid=%d cwd=%s\n", (int)getpid(), getcwd(p, 1000));
	free(p);
	return 42;
}
