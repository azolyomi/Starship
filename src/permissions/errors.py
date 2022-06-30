from discord.ext.commands import CommandError, NoPrivateMessage
from util.constants import PREFIX

ERROR_NOT_IN_GUILD = "This command can only be used in a server."
ERROR_NOT_FOUND = "Command not found. Use `{}help` for a list of commands.".format(PREFIX)
ERROR_NO_PERMISSION = "You do not have permission to use this command."
ERROR_NOT_ADMIN = "You must have an `admin` role configured with the bot to use this command."
ERROR_NOT_MOD = "You must have a `mod` role configured with the bot to use this command."
ERROR_NOT_STAFF = "You must have a `staff` role configured with the bot to use this command."



class StarshipPermissionsError(CommandError):
    message = ERROR_NO_PERMISSION

class NotModError(StarshipPermissionsError):
    message = ERROR_NOT_MOD

class NotAdminError(StarshipPermissionsError):
    message = ERROR_NOT_ADMIN

class NotStaffError(StarshipPermissionsError):
    message = ERROR_NOT_STAFF
