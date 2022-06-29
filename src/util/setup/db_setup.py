from database import db, ServerConfigs
from util.defaults import DEFAULT_SERVERCONFIG, DEFAULT_VCLESS_CATEGORIES

async def initialize_db_serverconfig(ctx, control_channel_id, raiding_channel_ids, admin_role_id, log_channel_id):
    data = DEFAULT_SERVERCONFIG

    data["guildID"] = ctx.guild.id
    data["adminroles"].append(admin_role_id)
    data["modroles"].append(admin_role_id)
    data["staffroles"].append(admin_role_id)
    data["log_channel_id"] = log_channel_id
    data["raiding"]["vcless"]["control"]["channel_id"] = control_channel_id
    for raidType in raiding_channel_ids.keys():
        data["raiding"]["vcless"]["categories"][raidType]["channel_id"] = raiding_channel_ids[raidType]

    result = db.ServerConfigs.insert_one(data)
    if (result.inserted_id is not None):
        ServerConfigs[ctx.guild.id] = data
        return True
    return False