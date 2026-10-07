/* greet — an interactive program for the terminal: asks for a name on a
 * line of its own prompt, answers, and reports what it knows about the
 * terminal. Exits with 0 if its standard streams are the terminal and the
 * terminal has a size, 1 otherwise. */

#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <termios.h>
#include <unistd.h>

int main(void)
{
	char name[128];
	printf("What is your name? ");
	fflush(stdout);
	if (!fgets(name, sizeof name, stdin)) {
		printf("\nno name (end of input)\n");
		return 1;
	}
	name[strcspn(name, "\n")] = 0;
	printf("Hello, %s! Welcome to C on Veda.\n", name);

	struct winsize ws = { 0 };
	struct termios t;
	int tty = isatty(0) && isatty(1) && isatty(2);
	int sized = ioctl(1, TIOCGWINSZ, &ws) == 0 && ws.ws_row > 0 && ws.ws_col > 0;
	int modes = tcgetattr(0, &t) == 0 && (t.c_lflag & ICANON) && (t.c_lflag & ECHO);
	printf("terminal: %s, %dx%d, %s\n", tty ? "yes" : "no", ws.ws_col, ws.ws_row,
	       modes ? "canonical with echo" : "other modes");
	return tty && sized && modes ? 0 : 1;
}
