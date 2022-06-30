from discord.ext.commands import CommandError, NoPrivateMessage
from util.constants import PREFIX

ERROR_NOT_IN_GUILD = "This command can only be used in a server."
ERROR_NOT_FOUND = "Command not found. Use `{}help` for a list of commands.".format(PREFIX)
ERROR_NO_PERMISSION = "You do not have permission to use this command."

def ERROR_MISSING_ROLE(role):
    return "You must have an `{}` role configured with the bot to use this command.".format(role)

class StarshipPermissionsError(CommandError):
    message = ERROR_NO_PERMISSION

class StarshipRoleMissingError(StarshipPermissionsError):
    def __init__(self, role):
        self.message = ERROR_MISSING_ROLE(role)
