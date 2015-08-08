#ifndef __KSTARTUP_H
#define __KSTARTUP_H

long long int now_init(void);
void now_save(long long int now);
int watchdog_set(int ms);
void watchdog_clear(int id);
int send_rpc(int service, const char *tag, ...);
void lognonl(const char *fmt, ...);
void log(const char *fmt, ...);

#endif /* __KSTARTUP_H */
