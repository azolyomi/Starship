from database import ServerConfigs
from util.defaults import DEFAULT_SERVERCONFIG

def create_local_config(guildID):
    data = DEFAULT_SERVERCONFIG
    data["guildID"] = guildID
    ServerConfigs[guildID] = data
    