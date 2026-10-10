data class Address(val city: String)

fun cityOf(user: Address?): String {
    return user!!.city
}
